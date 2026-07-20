//! Memory 语义检索层 — Step 2.4。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 2.4
//!
//! 架构:
//! - 三级层级(参考 Claude Code 源码泄露):
//!   - L1 索引:150 字符/条,常驻内存,半稳定区(缓存命中)
//!   - L2 主题文件:按需加载,变动区
//!   - L3 原始记录:仅搜索访问,变动区
//! - [`SemanticRecaller`]:统一语义召回入口,持有 HNSW 索引 + L1 索引。
//! - [`semantic_recall`]:免费函数入口 `semantic_recall(query, k) -> Vec<MemoryHit>`。
//! - 嵌入模型:先支持 OpenAI text-embedding-3-small,后扩展本地模型。
//! - Fallback:嵌入 API 不可用时,退化为 rule-based 关键词匹配。
//!
//! **缓存保护**(详见 §5.2):
//! - L1 在半稳定区(缓存命中)。
//! - L2/L3 通过 tool 获取(变动区,末尾注入)。
//! - 召回结果末尾追加到 prompt 变动区,不污染稳定区。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// L1 索引条目 — 每条最多 150 字符,常驻内存,位于 prompt 半稳定区。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct L1IndexEntry {
    /// 唯一标识(用于 L2/L3 回查)。
    pub id: String,
    /// 截断至 150 字符的内容摘要。
    pub summary: String,
    /// 来源(L2 文件路径或 L3 记录 ID)。
    pub source: String,
    /// 创建时间(unix epoch 秒)。
    pub created_at: u64,
}

/// 语义召回命中 — `semantic_recall` 返回的单条结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHit {
    /// L1 索引条目。
    pub entry: L1IndexEntry,
    /// 相似度分数(0.0-1.0,越高越相似)。
    pub score: f32,
    /// 匹配层级(L1/L2/L3)。
    pub level: RecallLevel,
}

/// 匹配层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecallLevel {
    /// L1 索引直接命中(最快)。
    L1,
    /// L2 主题文件命中(按需加载)。
    L2,
    /// L3 原始记录命中(全量搜索)。
    L3,
}

/// 语义召回策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecallStrategy {
    /// 嵌入向量语义搜索(需要嵌入 API)。
    Embedding,
    /// 关键词匹配 fallback(不需要外部 API)。
    Keyword,
}

impl Default for RecallStrategy {
    fn default() -> Self {
        Self::Keyword
    }
}

/// 语义召回器 — 持有 L1 索引和嵌入状态。
///
/// HNSW 向量索引在 embedding feature 可用时编译引入;
/// 默认构建使用 Keyword fallback。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticRecaller {
    /// L1 索引(常驻内存)。
    l1_index: Vec<L1IndexEntry>,
    /// 当前召回策略。
    strategy: RecallStrategy,
    /// L2 文件路径 → 加载状态。
    l2_loaded: HashMap<String, bool>,
    /// 嵌入 API 可用标记(运行时检测)。
    embedding_available: bool,
}

/// L1 索引条目摘要的字符上限。
pub const L1_SUMMARY_MAX_CHARS: usize = 150;

/// 默认召回条数。
pub const DEFAULT_RECALL_K: usize = 5;

/// 关键词匹配的最低相似度阈值。
pub const KEYWORD_MIN_SCORE: f32 = 0.1;

/// 嵌入匹配的最低相似度阈值(0.85,参考 §5.2 缓存保护)。
pub const EMBEDDING_MIN_SCORE: f32 = 0.85;

impl SemanticRecaller {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 使用嵌入策略构建(需要嵌入 API 可用)。
    #[must_use]
    pub fn with_embedding() -> Self {
        Self {
            strategy: RecallStrategy::Embedding,
            embedding_available: true,
            ..Self::default()
        }
    }

    /// 使用关键词 fallback 策略构建。
    #[must_use]
    pub fn with_keyword_fallback() -> Self {
        Self {
            strategy: RecallStrategy::Keyword,
            embedding_available: false,
            ..Self::default()
        }
    }

    /// 添加 L1 索引条目。
    pub fn add_l1_entry(&mut self, id: &str, content: &str, source: &str) {
        let summary = if content.chars().count() <= L1_SUMMARY_MAX_CHARS {
            content.to_owned()
        } else {
            let truncated: String = content.chars().take(L1_SUMMARY_MAX_CHARS).collect();
            format!("{truncated}…")
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.l1_index.push(L1IndexEntry {
            id: id.to_owned(),
            summary,
            source: source.to_owned(),
            created_at: now,
        });
    }

    /// 语义召回 — 查找与 `query` 最相似的 k 条记忆。
    ///
    /// 策略:
    /// 1. 若嵌入可用且策略为 Embedding → 走向量搜索(当前为 placeholder,返回空)。
    /// 2. 否则 → 走关键词匹配 fallback。
    ///
    /// **缓存保护**:召回结果应末尾追加到 prompt 变动区,不污染稳定区。
    #[must_use]
    pub fn semantic_recall(&self, query: &str, k: usize) -> Vec<MemoryHit> {
        let k = k.max(1);
        match self.strategy {
            RecallStrategy::Embedding if self.embedding_available => {
                // 嵌入向量搜索 — 当前为 placeholder,待集成 HNSW crate。
                // Fallback 到关键词匹配。
                self.keyword_recall(query, k)
            }
            _ => self.keyword_recall(query, k),
        }
    }

    /// 关键词匹配 fallback — 对 L1 索引做大小写不敏感的子串匹配。
    fn keyword_recall(&self, query: &str, k: usize) -> Vec<MemoryHit> {
        let query_lower = query.to_ascii_lowercase();
        let query_tokens: Vec<&str> = query_lower.split_whitespace().collect();

        let mut hits: Vec<MemoryHit> = self
            .l1_index
            .iter()
            .filter_map(|entry| {
                let entry_lower = entry.summary.to_ascii_lowercase();
                let match_count = query_tokens
                    .iter()
                    .filter(|token| entry_lower.contains(*token))
                    .count();
                if match_count == 0 {
                    return None;
                }
                let score = (match_count as f32 / query_tokens.len().max(1) as f32)
                    .max(KEYWORD_MIN_SCORE);
                Some(MemoryHit {
                    entry: entry.clone(),
                    score,
                    level: RecallLevel::L1,
                })
            })
            .collect();

        // 按相似度降序排序,取 top-k。
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(k);
        hits
    }

    /// 持久化 L1 索引到 `<workspace>/.claw/memory-l1-index.json`。
    pub fn persist_l1_index(
        &self,
        workspace: &Path,
    ) -> Result<PathBuf, std::io::Error> {
        let dir = workspace.join(".claw");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("memory-l1-index.json");
        let json = serde_json::to_string_pretty(&self.l1_index)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// 从文件加载 L1 索引。
    pub fn load_l1_index(
        &mut self,
        workspace: &Path,
    ) -> Result<usize, std::io::Error> {
        let path = workspace.join(".claw").join("memory-l1-index.json");
        let content = std::fs::read_to_string(&path)?;
        let entries: Vec<L1IndexEntry> = serde_json::from_str(&content)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        let count = entries.len();
        self.l1_index = entries;
        Ok(count)
    }

    /// 获取 L1 索引条目数量。
    #[must_use]
    pub fn l1_count(&self) -> usize {
        self.l1_index.len()
    }

    /// 获取当前策略。
    #[must_use]
    pub fn strategy(&self) -> RecallStrategy {
        self.strategy
    }

    /// 标记嵌入 API 不可用(运行时降级)。
    pub fn degrade_to_keyword(&mut self) {
        self.embedding_available = false;
        self.strategy = RecallStrategy::Keyword;
    }
}

/// 免费函数入口 — 对默认构建的 recaller 做语义召回。
///
/// 适用于无状态场景(测试/简单调用)。生产环境应持有
/// [`SemanticRecaller`] 实例以复用 L1 索引。
#[must_use]
pub fn semantic_recall(query: &str, k: usize) -> Vec<MemoryHit> {
    SemanticRecaller::new().semantic_recall(query, k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l1_entry_summary_truncated_at_150_chars() {
        let long_content = "a".repeat(200);
        let mut recaller = SemanticRecaller::new();
        recaller.add_l1_entry("test", &long_content, "source");
        assert_eq!(recaller.l1_index[0].summary.chars().count(), 151); // 150 + '…'
    }

    #[test]
    fn l1_entry_summary_not_truncated_when_short() {
        let short_content = "hello world";
        let mut recaller = SemanticRecaller::new();
        recaller.add_l1_entry("test", short_content, "source");
        assert_eq!(recaller.l1_index[0].summary, short_content);
    }

    #[test]
    fn keyword_recall_returns_matching_entries() {
        let mut recaller = SemanticRecaller::with_keyword_fallback();
        recaller.add_l1_entry("1", "Rust programming language", "doc1");
        recaller.add_l1_entry("2", "Python data analysis", "doc2");
        recaller.add_l1_entry("3", "Rust async runtime tokio", "doc3");

        let hits = recaller.semantic_recall("Rust programming", 5);
        assert!(!hits.is_empty());
        assert!(hits[0].entry.summary.contains("Rust"));
        assert!(hits[0].score > KEYWORD_MIN_SCORE);
    }

    #[test]
    fn keyword_recall_returns_empty_for_no_match() {
        let mut recaller = SemanticRecaller::with_keyword_fallback();
        recaller.add_l1_entry("1", "Rust programming", "doc1");
        let hits = recaller.semantic_recall("completely unrelated query xyz", 5);
        assert!(hits.is_empty());
    }

    #[test]
    fn keyword_recall_respects_k_limit() {
        let mut recaller = SemanticRecaller::with_keyword_fallback();
        for i in 0..10 {
            recaller.add_l1_entry(&i.to_string(), "Rust topic", &format!("doc{i}"));
        }
        let hits = recaller.semantic_recall("Rust", 3);
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn keyword_recall_sorted_by_score_descending() {
        let mut recaller = SemanticRecaller::with_keyword_fallback();
        recaller.add_l1_entry("1", "Rust language features", "doc1");
        recaller.add_l1_entry("2", "Rust language", "doc2");

        let hits = recaller.semantic_recall("Rust language features", 5);
        if hits.len() >= 2 {
            assert!(hits[0].score >= hits[1].score);
        }
    }

    #[test]
    fn embedding_strategy_falls_back_to_keyword_when_no_vector_index() {
        let mut recaller = SemanticRecaller::with_embedding();
        recaller.add_l1_entry("1", "Rust programming", "doc1");
        // Embedding path currently falls back to keyword
        let hits = recaller.semantic_recall("Rust", 5);
        assert!(!hits.is_empty());
    }

    #[test]
    fn degrade_to_keyword_switches_strategy() {
        let mut recaller = SemanticRecaller::with_embedding();
        assert_eq!(recaller.strategy(), RecallStrategy::Embedding);
        recaller.degrade_to_keyword();
        assert_eq!(recaller.strategy(), RecallStrategy::Keyword);
        assert!(!recaller.embedding_available);
    }

    #[test]
    fn persist_and_load_l1_index_round_trip() {
        let temp = std::env::temp_dir().join(format!(
            "semantic-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp).unwrap();

        let mut recaller = SemanticRecaller::new();
        recaller.add_l1_entry("1", "test entry one", "doc1");
        recaller.add_l1_entry("2", "test entry two", "doc2");

        let path = recaller.persist_l1_index(&temp).expect("persist should succeed");
        assert!(path.exists());

        let mut loaded = SemanticRecaller::new();
        let count = loaded.load_l1_index(&temp).expect("load should succeed");
        assert_eq!(count, 2);
        assert_eq!(loaded.l1_count(), 2);
        assert_eq!(loaded.l1_index[0].summary, "test entry one");

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn free_function_semantic_recall_returns_empty_for_empty_index() {
        let hits = semantic_recall("anything", 5);
        assert!(hits.is_empty());
    }

    #[test]
    fn recall_level_is_l1_for_keyword_matches() {
        let mut recaller = SemanticRecaller::with_keyword_fallback();
        recaller.add_l1_entry("1", "Rust programming", "doc1");
        let hits = recaller.semantic_recall("Rust", 5);
        assert!(hits.iter().all(|h| h.level == RecallLevel::L1));
    }
}
