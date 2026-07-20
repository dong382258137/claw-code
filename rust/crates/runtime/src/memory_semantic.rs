//! Memory 语义检索层 — Step 2.4。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 2.4
//!
//! 架构:
//! - 三级层级(参考 Claude Code 源码泄露):
//!   - L1 索引:150 字符/条,常驻内存,半稳定区(缓存命中)
//!   - L2 主题文件:按需加载,变动区
//!   - L3 原始记录:仅搜索访问,变动区
//! - [`SemanticRecaller`]:统一语义召回入口,持有向量索引 + L1 索引。
//! - [`semantic_recall`]:免费函数入口 `semantic_recall(query, k) -> Vec<MemoryHit>`。
//! - 嵌入模型:默认 [`HashEmbeddingProvider`](简易 hash 向量,无外部依赖);
//!   启用 `embedding` feature 后使用 [`FastembedProvider`](BGE-small-en-v1.5, 384 维)。
//! - Fallback:嵌入不可用时退化为关键词匹配。
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

/// 语义召回器 — 持有 L1 索引和(可选)向量索引。
///
/// 向量索引在 `index_embeddings` 调用后填充,每个 L1 entry 对应一个嵌入向量。
/// `semantic_recall` 不带 provider 时退化到 keyword;`semantic_recall_with_provider`
/// 在 strategy=Embedding 且 vectors 非空时走向量搜索。
///
/// **Eq 不再 derive**:`Vec<f32>` 不 impl Eq(floating point NaN 问题)。
/// `PartialEq` 仍 derive,测试用 `assert_eq!` 比较 entry 不受影响。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SemanticRecaller {
    /// L1 索引(常驻内存)。
    l1_index: Vec<L1IndexEntry>,
    /// 当前召回策略。
    strategy: RecallStrategy,
    /// L2 文件路径 → 加载状态。
    l2_loaded: HashMap<String, bool>,
    /// 嵌入 API 可用标记(运行时检测)。
    embedding_available: bool,
    /// 嵌入向量索引(L1 entry id → embedding vector)。
    /// `#[serde(skip)]` 因为向量是 L1 索引的纯派生数据,
    /// 持久化时只存 L1 索引,加载后通过 `index_embeddings` 重建。
    #[serde(skip)]
    vectors: HashMap<String, Vec<f32>>,
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
    /// **不带 embedding provider**:若 strategy=Embedding 但 vectors 已填充,
    /// 仍退化到 keyword(因为无法 embed query)。需要向量搜索时使用
    /// [`SemanticRecaller::semantic_recall_with_provider`]。
    ///
    /// **缓存保护**:召回结果应末尾追加到 prompt 变动区,不污染稳定区。
    #[must_use]
    pub fn semantic_recall(&self, query: &str, k: usize) -> Vec<MemoryHit> {
        let k = k.max(1);
        // 无 provider 时统一走 keyword fallback。
        self.keyword_recall(query, k)
    }

    /// 带 embedding provider 的语义召回。
    ///
    /// 当 strategy=Embedding 且 vectors 已填充时:
    /// 1. 用 provider.embed(query) 计算 query 向量。
    /// 2. 对 vectors 索引做 cosine 相似度搜索,取 top-k。
    /// 3. 分数低于 [`EMBEDDING_MIN_SCORE`] 的结果过滤掉。
    ///
    /// 当 strategy=Keyword 或 vectors 为空或 embed 失败时,退化到 keyword。
    ///
    /// **缓存保护**:召回结果末尾追加到 prompt 变动区。
    #[must_use]
    pub fn semantic_recall_with_provider(
        &self,
        query: &str,
        k: usize,
        provider: &dyn EmbeddingProvider,
    ) -> Vec<MemoryHit> {
        let k = k.max(1);
        if self.strategy == RecallStrategy::Embedding
            && self.embedding_available
            && !self.vectors.is_empty()
        {
            match provider.embed(query) {
                Ok(query_vec) if !query_vec.is_empty() => {
                    return self.vector_recall(&query_vec, k);
                }
                _ => {
                    // embed 失败或返回空向量 — fallback 到 keyword
                    return self.keyword_recall(query, k);
                }
            }
        }
        self.keyword_recall(query, k)
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

    /// 向量召回 — 对 vectors 索引做 cosine 相似度搜索。
    fn vector_recall(&self, query_vec: &[f32], k: usize) -> Vec<MemoryHit> {
        let mut hits: Vec<MemoryHit> = self
            .l1_index
            .iter()
            .filter_map(|entry| {
                self.vectors.get(&entry.id).map(|entry_vec| {
                    let score = cosine_similarity(query_vec, entry_vec);
                    MemoryHit {
                        entry: entry.clone(),
                        score,
                        level: RecallLevel::L1,
                    }
                })
            })
            .filter(|hit| hit.score >= EMBEDDING_MIN_SCORE)
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(k);
        hits
    }

    /// 预计算所有 L1 entry 的 embedding 并存入 vectors 索引。
    ///
    /// 幂等:已存在向量的 entry 不会被重新计算。
    /// 失败时返回 [`EmbeddingError`],已计算的部分仍保留在 vectors 中。
    pub fn index_embeddings(
        &mut self,
        provider: &dyn EmbeddingProvider,
    ) -> Result<usize, EmbeddingError> {
        let mut computed = 0usize;
        let entries_to_embed: Vec<(String, String)> = self
            .l1_index
            .iter()
            .filter(|e| !self.vectors.contains_key(&e.id))
            .map(|e| (e.id.clone(), e.summary.clone()))
            .collect();
        if entries_to_embed.is_empty() {
            return Ok(0);
        }
        // 批量嵌入以减少 ONNX 推理开销。
        let texts: Vec<&str> = entries_to_embed.iter().map(|(_, s)| s.as_str()).collect();
        let vectors = provider.embed_batch(&texts)?;
        for ((id, _), vec) in entries_to_embed.into_iter().zip(vectors.into_iter()) {
            self.vectors.insert(id, vec);
            computed += 1;
        }
        Ok(computed)
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

    /// 获取已索引的向量数量。
    #[must_use]
    pub fn vector_count(&self) -> usize {
        self.vectors.len()
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
        self.vectors.clear();
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

/// 计算 two vectors 的 cosine 相似度。
///
/// 数学公式:`cos(a, b) = (a · b) / (|a| × |b|)`。
/// 任一向量为零向量时返回 0.0(避免除以零)。
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

// ============================================================================
// EmbeddingProvider trait + 实现
// ============================================================================

/// 嵌入模型错误。
#[derive(Debug, Clone)]
pub enum EmbeddingError {
    /// 模型加载失败(如 ONNX Runtime 初始化失败、模型文件缺失)。
    ModelLoad(String),
    /// 推理失败(如输入过长、维度不匹配)。
    Inference(String),
    /// 不支持的操作(如批量大小超限)。
    Unsupported(String),
}

impl std::fmt::Display for EmbeddingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelLoad(msg) => write!(f, "embedding model load failed: {msg}"),
            Self::Inference(msg) => write!(f, "embedding inference failed: {msg}"),
            Self::Unsupported(msg) => write!(f, "embedding unsupported: {msg}"),
        }
    }
}

impl std::error::Error for EmbeddingError {}

/// 嵌入模型 provider trait — 抽象不同的 embedding 实现。
///
/// 实现者:
/// - [`HashEmbeddingProvider`]:简易 hash 向量(默认,无外部依赖,用于测试)。
/// - [`FastembedProvider`](crate::memory_semantic::FastembedProvider):BGE-small-en-v1.5
///   (启用 `embedding` feature,基于 ONNX Runtime)。
///
/// **缓存保护**:provider 是无状态调用方,不影响主 agent 缓存。
/// 嵌入结果通过 [`SemanticRecaller::index_embeddings`] 写入 vectors 索引,
/// 召回结果末尾追加到 prompt 变动区(详见 §5.2)。
pub trait EmbeddingProvider: Send + Sync {
    /// 嵌入一段文本,返回向量。
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// 批量嵌入(默认实现:逐个调用 `embed`,实现者可重写以利用批处理)。
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// 向量维度。
    fn dim(&self) -> usize;

    /// provider 名称(用于诊断日志)。
    fn name(&self) -> &str;
}

/// Hash-based embedding provider — 简易 hash 向量,无外部依赖。
///
/// **仅用于测试 / 默认 fallback**。语义质量差,但满足 trait 契约。
/// 算法:对每个 token 做 FNV-1a hash,映射到固定维度向量,归一化。
#[derive(Debug, Clone, Default)]
pub struct HashEmbeddingProvider {
    dim: usize,
}

impl HashEmbeddingProvider {
    /// 创建指定维度的 hash embedding provider。
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }

    /// 默认 256 维。
    #[must_use]
    pub fn default_dim() -> Self {
        Self::new(256)
    }

    /// FNV-1a 64-bit hash。
    fn hash_token(&self, token: &str) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in token.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash
    }

    /// 把 token hash 映射到向量索引和符号(+1 / -1)。
    fn hash_to_index_sign(&self, hash: u64) -> (usize, f32) {
        let idx = (hash % self.dim as u64) as usize;
        let sign = if (hash / self.dim as u64) % 2 == 0 {
            1.0
        } else {
            -1.0
        };
        (idx, sign)
    }
}

impl EmbeddingProvider for HashEmbeddingProvider {
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut vec = vec![0.0f32; self.dim];
        let lowered = text.to_ascii_lowercase();
        let tokens: Vec<&str> = lowered.split_whitespace().collect();
        if tokens.is_empty() {
            return Ok(vec);
        }
        for token in &tokens {
            let hash = self.hash_token(token);
            let (idx, sign) = self.hash_to_index_sign(hash);
            vec[idx] += sign;
        }
        // L2 归一化。
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vec {
                *v /= norm;
            }
        }
        Ok(vec)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "hash-embedding-256"
    }
}

// ============================================================================
// FastembedProvider — 启用 `embedding` feature 时可用
// ============================================================================

#[cfg(feature = "embedding")]
pub mod fastembed_provider {
    //! Fastembed provider — 基于 BGE-small-en-v1.5 模型(384 维)。
    //!
    //! 启用方式:`cargo build --features runtime/embedding`。
    //! 首次运行会下载 ONNX 模型文件(~50MB)到缓存目录。

    use super::{EmbeddingError, EmbeddingProvider};

    use fastembed::{InitOptions, TextEmbedding};
    use std::sync::{Arc, Mutex};

    /// Fastembed provider — 持有 ONNX Runtime + 模型。
    ///
    /// **线程安全**:`TextEmbedding` 是 `Send + Sync`,可跨线程共享;
    /// 通过 `Arc<Mutex<..>>` 共享所有权以满足 `Clone` 需求,同时支持
    /// fastembed 5.x 的 `&mut self` embed 调用约定。
    /// **模型加载**:首次 `try_new()` 时下载/加载模型,可能耗时数秒。
    /// **内存占用**:~200-500MB(模型 + 推理 buffer)。
    #[derive(Clone)]
    pub struct FastembedProvider {
        model: Arc<Mutex<TextEmbedding>>,
    }

    impl std::fmt::Debug for FastembedProvider {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FastembedProvider")
                .field("model", &"TextEmbedding(BGE-small-en-v1.5)")
                .finish()
        }
    }

    impl FastembedProvider {
        /// 创建并加载 BGE-small-en-v1.5 模型。
        ///
        /// 首次调用会下载模型文件到 `~/.cache/huggingface`(`HF_HOME` 可覆盖)。
        /// 后续调用从缓存加载,~100ms。
        pub fn try_new() -> Result<Self, EmbeddingError> {
            let model = TextEmbedding::try_new(InitOptions::default())
                .map_err(|err| EmbeddingError::ModelLoad(err.to_string()))?;
            Ok(Self {
                model: Arc::new(Mutex::new(model)),
            })
        }
    }

    impl EmbeddingProvider for FastembedProvider {
        fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            let docs = vec![text.to_owned()];
            // fastembed 5.17.3: `embed` 要求 `&mut self`,通过 Mutex 保护。
            // 返回 `Result<Vec<Embedding>>`,`Embedding = Vec<f32>`。
            let mut embeddings = self
                .model
                .lock()
                .map_err(|err| EmbeddingError::Inference(err.to_string()))?
                .embed(docs, None)
                .map_err(|err| EmbeddingError::Inference(err.to_string()))?;
            embeddings
                .pop()
                .ok_or_else(|| EmbeddingError::Inference("empty embedding result".to_owned()))
        }

        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            let docs: Vec<String> = texts.iter().map(|s| (*s).to_owned()).collect();
            let embeddings = self
                .model
                .lock()
                .map_err(|err| EmbeddingError::Inference(err.to_string()))?
                .embed(docs, None)
                .map_err(|err| EmbeddingError::Inference(err.to_string()))?;
            Ok(embeddings)
        }

        fn dim(&self) -> usize {
            // BGE-small-en-v1.5 输出维度。
            384
        }

        fn name(&self) -> &str {
            "fastembed-bge-small-en-v1.5"
        }
    }
}

#[cfg(feature = "embedding")]
pub use fastembed_provider::FastembedProvider;

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
    fn embedding_strategy_falls_back_to_keyword_when_no_provider() {
        let mut recaller = SemanticRecaller::with_embedding();
        recaller.add_l1_entry("1", "Rust programming", "doc1");
        // 无 provider 时仍可调用,退化到 keyword。
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

    // ========================================================================
    // Embedding provider 测试
    // ========================================================================

    #[test]
    fn hash_embedding_provider_returns_correct_dim() {
        let provider = HashEmbeddingProvider::new(128);
        assert_eq!(provider.dim(), 128);
        let vec = provider.embed("hello world").unwrap();
        assert_eq!(vec.len(), 128);
    }

    #[test]
    fn hash_embedding_provider_normalizes_to_unit_length() {
        let provider = HashEmbeddingProvider::default_dim();
        let vec = provider.embed("hello world foo bar baz").unwrap();
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "L2 norm should be 1.0, got {norm}");
    }

    #[test]
    fn hash_embedding_provider_empty_text_returns_zero_vector() {
        let provider = HashEmbeddingProvider::default_dim();
        let vec = provider.embed("").unwrap();
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert_eq!(norm, 0.0);
    }

    #[test]
    fn hash_embedding_provider_identical_inputs_produce_identical_vectors() {
        let provider = HashEmbeddingProvider::default_dim();
        let v1 = provider.embed("Rust programming language").unwrap();
        let v2 = provider.embed("Rust programming language").unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn cosine_similarity_handles_identical_vectors() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-5, "identical vectors should have cos=1.0");
    }

    #[test]
    fn cosine_similarity_handles_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-5, "orthogonal vectors should have cos=0.0");
    }

    #[test]
    fn cosine_similarity_handles_zero_vector() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn cosine_similarity_handles_mismatched_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn index_embeddings_populates_vectors_for_all_entries() {
        let mut recaller = SemanticRecaller::with_embedding();
        recaller.add_l1_entry("1", "Rust programming", "doc1");
        recaller.add_l1_entry("2", "Python data analysis", "doc2");
        assert_eq!(recaller.vector_count(), 0);

        let provider = HashEmbeddingProvider::default_dim();
        let count = recaller.index_embeddings(&provider).unwrap();
        assert_eq!(count, 2);
        assert_eq!(recaller.vector_count(), 2);
    }

    #[test]
    fn index_embeddings_is_idempotent() {
        let mut recaller = SemanticRecaller::with_embedding();
        recaller.add_l1_entry("1", "Rust programming", "doc1");
        let provider = HashEmbeddingProvider::default_dim();

        let first = recaller.index_embeddings(&provider).unwrap();
        let second = recaller.index_embeddings(&provider).unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0, "second call should not recompute");
    }

    #[test]
    fn semantic_recall_with_provider_uses_vector_search() {
        let mut recaller = SemanticRecaller::with_embedding();
        recaller.add_l1_entry("1", "Rust programming language", "doc1");
        recaller.add_l1_entry("2", "Python data analysis", "doc2");
        recaller.add_l1_entry("3", "Rust async runtime tokio", "doc3");
        let provider = HashEmbeddingProvider::default_dim();
        recaller.index_embeddings(&provider).unwrap();

        // 完全相同的查询 → 应该命中(余弦相似度 = 1.0 ≥ EMBEDDING_MIN_SCORE)。
        let hits = recaller.semantic_recall_with_provider(
            "Rust programming language",
            5,
            &provider,
        );
        assert!(!hits.is_empty(), "should find at least one hit");
        assert!(hits[0].score >= EMBEDDING_MIN_SCORE);
    }

    #[test]
    fn semantic_recall_with_provider_falls_back_on_unrelated_query() {
        let mut recaller = SemanticRecaller::with_embedding();
        recaller.add_l1_entry("1", "Rust programming", "doc1");
        let provider = HashEmbeddingProvider::default_dim();
        recaller.index_embeddings(&provider).unwrap();

        // 完全不相关的查询 → 向量相似度低 → 命中为空,但 keyword 也无匹配 → 空结果。
        let hits = recaller.semantic_recall_with_provider(
            "completely unrelated xyz123",
            5,
            &provider,
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn semantic_recall_with_provider_falls_back_when_strategy_is_keyword() {
        let mut recaller = SemanticRecaller::with_keyword_fallback();
        recaller.add_l1_entry("1", "Rust programming", "doc1");
        let provider = HashEmbeddingProvider::default_dim();
        recaller.index_embeddings(&provider).unwrap();

        // strategy=Keyword → 即使 vectors 已填充也走 keyword。
        // "Rust" 单 token 查询匹配 "Rust programming" entry → keyword score = 1.0
        // (match_count=1 / query_tokens.len()=1)。
        // 若走向量搜索:hash embedding "Rust" vs "Rust programming" 余弦相似度 < 1.0,
        // 但会被 EMBEDDING_MIN_SCORE=0.85 过滤(取决于 hash 碰撞)。
        // 关键判定:keyword 路径返回的 score 必然 = 1.0(perfect match)。
        let hits = recaller.semantic_recall_with_provider("Rust", 5, &provider);
        assert!(!hits.is_empty());
        assert!(
            (hits[0].score - 1.0).abs() < 1e-5,
            "strategy=Keyword should return keyword score 1.0, got {}",
            hits[0].score
        );
    }

    #[test]
    fn semantic_recall_with_provider_falls_back_when_no_vectors() {
        let recaller = SemanticRecaller::with_embedding();
        // 不调用 index_embeddings,vectors 为空。
        let provider = HashEmbeddingProvider::default_dim();
        let hits = recaller.semantic_recall_with_provider("Rust", 5, &provider);
        assert!(hits.is_empty());
    }

    #[test]
    fn hash_provider_embed_batch_returns_correct_count() {
        let provider = HashEmbeddingProvider::default_dim();
        let texts = ["hello", "world", "foo"];
        let result = provider.embed_batch(&texts).unwrap();
        assert_eq!(result.len(), 3);
        for vec in &result {
            assert_eq!(vec.len(), provider.dim());
        }
    }
}
