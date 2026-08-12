# 会话历史混合检索(FTS5 + 向量 RRF 融合)实现方案

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 `session_search` 工具增加词法(FTS5 BM25)+ 语义(向量)双路检索，用 RRF(Reciprocal Rank Fusion)融合排序，提升跨会话历史召回质量。

**Architecture:** 在现有 `HistoryIndex`(SQLite FTS5)上叠加稠密检索路径。`index_message` 注入 embedding provider 后为每条消息增量计算向量，存 `history_vectors` 表(rowid 关联)。`hybrid_search` 词法取 top_k×2 + 稠密取 top_k×2 → RRF(k=60)融合 → top_k。无 embedder / 嵌入失败 / 向量为空时自动回退纯 FTS5，**行为与现状完全一致**。

**Tech Stack:** Rust、rusqlite 0.31(bundled FTS5)、fastembed 5(BGE-small-en-v1.5, 384 维, `embedding` feature)、hash-embedding 测试替身。

**依据:** True Memory 论文(arXiv:2605.04897)检索中心架构 —— 词法+稠密双路 + RRF 融合是检索管线主干，且论文诊断"检索是瓶颈而非存储"。

---

## 背景与现状(代码事实)

- [session.rs](d:/claw-code-src/rust/crates/runtime/src/session.rs#L805-L834) `append_persisted_message` 把每条消息逐字写入 FTS5(已经是 True Memory 主张的 verbatim substrate)。
- [history_search.rs](d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L135-L179) `search()` 只有 BM25 单路；`decision` role 加权 rank×2.0；CJK 拆字已实现。
- [conversation.rs](d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L4343-L4424) `execute_session_search` 调 `history_index.search()`；`SESSION_SEARCH_TOOL_SPEC` 描述为纯全文搜索。
- [lib.rs](d:/claw-code-src/rust/crates/runtime/src/lib.rs#L390-L415) `build_embedding_provider()` 返回 `Box<dyn EmbeddingProvider>`，目前仅 [memory.rs](d:/claw-code-src/rust/crates/runtime/src/memory.rs#L338) 调用。
- [memory_semantic.rs](d:/claw-code-src/rust/crates/runtime/src/memory_semantic.rs#L84-L122) `EmbeddingProviderRef`(包装 `Arc<dyn EmbeddingProvider>`)与 `cosine_similarity` 可复用(需放宽可见性)。
- [session_mgr.rs](d:/claw-code-src/rust/crates/rusty-claude-cli/src/session_mgr.rs#L1114-L1145) `new_cli_session` / `new_cli_session_with_roots` 是 HistoryIndex 唯一生产接线点。
- `rusty-claude-cli` 默认 features 含 `embedding`(见其 Cargo.toml:67-70)，即生产二进制已加载 BGE 模型。

## 关键设计决策

1. **Schema v3 → v4(纯增量)**:新增 `history_vectors(message_id INTEGER PRIMARY KEY, vector BLOB)` 表，`message_id` = FTS5 行 rowid。老库打开即建表，**不回溯补向量**(历史消息向量随新消息逐步累积，为已知限制)。
2. **单例 embedder**:`build_embedding_provider()` 改为 `OnceLock` 进程级单例返回 `Arc<...>`，PersistentMemory 与 HistoryIndex 共享同一 BGE 实例(避免重复加载 ~300MB)。`Arc::from(Arc)` 为恒等转换，[memory.rs](d:/claw-code-src/rust/crates/runtime/src/memory.rs#L339) 调用点零改动。
3. **向量在锁外计算**:`index_message` 先 embed(不持 mutex，避免阻塞检索)，再持锁写入。
4. **巨型内容跳过嵌入**:内容 > `MAX_EMBED_CHARS=4096` 字符不嵌入(工具 dump 语义价值低)；词法路径不受影响。
5. **融合键**:`(session_id, message_index, role)` 标识同一逻辑消息；双列命中叠加 RRF 贡献。
6. **rank 语义变化**:`hybrid_search` 返回的 `rank` 为 RRF 融合分数(**越高越相关**)，与 `search()` 的 BM25 rank(**越低越相关**)不同；`search()` 保持原语义不动。
7. **回退链**:无 embedder → 纯词法；查询嵌入失败 → 纯词法；dense 列表为空 → 纯词法。**默认构建(未开 embedding)零行为变化。**

## 文件结构

| 文件 | 职责 | 变更类型 |
|---|---|---|
| `rust/crates/runtime/src/history_search.rs` | schema v4、embedder 注入、向量存储、dense_search、rrf_merge、hybrid_search、clear_session 向量清理、迁移兼容 | 修改(核心) |
| `rust/crates/runtime/src/memory_semantic.rs` | `EmbeddingProviderRef` 放宽为 `pub(crate)` + 增加 `new()`/`provider()` 访问器 | 修改(小) |
| `rust/crates/runtime/src/lib.rs` | `build_embedding_provider()` 改为 OnceLock 单例返回 `Arc` | 修改(小) |
| `rust/crates/runtime/src/conversation.rs` | `execute_session_search` 路由 `hybrid_search`；tool spec 描述更新 | 修改(小) |
| `rust/crates/rusty-claude-cli/src/session_mgr.rs` | 两个会话工厂注入共享 embedder | 修改(小) |

---

## Task 1: `build_embedding_provider` 改为进程级单例

**Files:**
- Modify: `rust/crates/runtime/src/lib.rs:390-415`
- Verify: `rust/crates/runtime/src/memory.rs:338-357`(应零改动)

- [ ] **Step 1: 改 `build_embedding_provider` 签名与实现**

将 [lib.rs](d:/claw-code-src/rust/crates/runtime/src/lib.rs#L377-L415) 中的工厂函数整体替换为:

```rust
// ── Embedding runtime factory ──
// Step 4.x: 将 embedding provider 的创建集中在一个工厂函数中,供 PersistentMemory、
// TraceAnalyzer 等消费者注入。feature `embedding` 开启时优先使用 FastembedProvider
// (BGE-small-en-v1.5,384 维),创建失败则自动降级到 HashEmbeddingProvider。
//
// v4:改为进程级 OnceLock 单例并返回 `Arc`。PersistentMemory 与 HistoryIndex
// 共用同一模型实例,避免 embedding feature 下重复加载 ONNX 模型(~300MB/份)。
// 首次调用可能耗时数秒(模型加载/下载),后续调用为 ~100ms。

/// 根据编译 feature 创建(或复用)进程级共享的 embedding provider。
///
/// - `feature = "embedding"` 开启且 FastembedProvider 初始化成功:返回 BGE-small 384 维。
/// - `feature = "embedding"` 开启但初始化失败(如模型下载失败):自动降级为 HashEmbeddingProvider。
/// - `feature = "embedding"` 未开启:返回 None(调用方应使用 keyword fallback)。
///
/// 返回 `None` 不表示错误,调用方应检测并退化为关键词匹配。
#[must_use]
pub fn build_embedding_provider() -> Option<Arc<dyn EmbeddingProvider + Send + Sync>> {
    static PROVIDER: OnceLock<Option<Arc<dyn EmbeddingProvider + Send + Sync>>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| {
            #[cfg(feature = "embedding")]
            {
                match memory_semantic::fastembed_provider::FastembedProvider::try_new() {
                    Ok(provider) => {
                        eprintln!(
                            "embedding provider: fastembed ({}-dim BGE-small-en-v1.5)",
                            provider.dim()
                        );
                        Some(Arc::new(provider) as Arc<dyn EmbeddingProvider + Send + Sync>)
                    }
                    Err(e) => {
                        eprintln!(
                            "fastembed init failed ({}), falling back to hash embedding",
                            e
                        );
                        Some(Arc::new(HashEmbeddingProvider::default_dim())
                            as Arc<dyn EmbeddingProvider + Send + Sync>)
                    }
                }
            }
            #[cfg(not(feature = "embedding"))]
            {
                None
            }
        })
        .clone()
}
```

- [ ] **Step 2: 补齐 import**

确认 [lib.rs](d:/claw-code-src/rust/crates/runtime/src/lib.rs) 顶部已有 `use std::sync::{Arc, OnceLock};` 与 `use crate::memory_semantic::{EmbeddingProvider, HashEmbeddingProvider};`；缺失则补上(注意现有 `EmbeddingProvider` 与 `HashEmbeddingProvider` 的引入方式，`pub use` 行 184 附近的 import 区)。

- [ ] **Step 3: 编译验证**

Run: `cargo check -p runtime`
Expected: 编译通过。若 [memory.rs](d:/claw-code-src/rust/crates/runtime/src/memory.rs#L339) 的 `Arc::from(provider)` 报错(旧签名是 Box)，将 `Arc::from(provider)` 改为 `provider` —— 预期不报错，因为 `Arc::from(Arc)` 是恒等转换。

- [ ] **Step 4: 提交**

```bash
git add rust/crates/runtime/src/lib.rs
git commit -m "refactor(runtime): build_embedding_provider 改为进程级 OnceLock 单例(返回 Arc)"
```

---

## Task 2: `EmbeddingProviderRef` 放宽可见性 + 访问器

**Files:**
- Modify: `rust/crates/runtime/src/memory_semantic.rs:84-99`

- [ ] **Step 1: 改 struct 可见性并加访问器**

将 [memory_semantic.rs](d:/claw-code-src/rust/crates/runtime/src/memory_semantic.rs#L84-L99) 的 `struct EmbeddingProviderRef` 改为 `pub(crate)`，并在其 impl 块内新增两个方法:

```rust
#[derive(Clone, Default)]
pub(crate) struct EmbeddingProviderRef(Option<Arc<dyn EmbeddingProvider + Send + Sync>>);

impl EmbeddingProviderRef {
    /// 包装一个新的 provider(供 HistoryIndex 注入)。
    pub(crate) fn new(provider: Arc<dyn EmbeddingProvider + Send + Sync>) -> Self {
        Self(Some(provider))
    }

    /// 借出底层 provider(若已注入)。
    pub(crate) fn provider(&self) -> Option<&(dyn EmbeddingProvider + Send + Sync)> {
        self.0.as_deref()
    }
}
```

(现有 `with_embedding_provider` 内直接构造 `EmbeddingProviderRef(Some(provider))` 的调用点在同一模块，不受影响。)

- [ ] **Step 2: 编译验证**

Run: `cargo check -p runtime`
Expected: 编译通过。

- [ ] **Step 3: 提交**

```bash
git add rust/crates/runtime/src/memory_semantic.rs
git commit -m "refactor(runtime): EmbeddingProviderRef 放宽为 pub(crate) 并新增 new/provider 访问器"
```

---

## Task 3: HistoryIndex 增加 embedder 字段 + schema v4

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs:29-83`

- [ ] **Step 1: 改 struct + open()**

将 struct 与 `open()` 的 `Self{..}` 构造替换为:

```rust
pub struct HistoryIndex {
    conn: Mutex<Connection>,
    /// 可选的稠密检索 embedder。注入后 `index_message` 为每条消息增量
    /// 计算向量存入 `history_vectors` 表,`hybrid_search` 走词法+稠密双路。
    /// 未注入时行为与纯 FTS5 `search` 完全一致。
    embedder: EmbeddingProviderRef,
}
```

`open()` 中把 schema_version 升到 `'4'`，并新增 `history_vectors` 建表(放在 `CREATE VIRTUAL TABLE` 之后):

```rust
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS history_meta (\
                 key TEXT PRIMARY KEY,\
                 value TEXT NOT NULL\
             );\
             INSERT OR REPLACE INTO history_meta (key, value)\
                 VALUES ('schema_version', '4');\
             CREATE VIRTUAL TABLE IF NOT EXISTS history USING fts5(\
                 content,\
                 content_raw UNINDEXED,\
                 session_id UNINDEXED,\
                 role UNINDEXED,\
                 message_index UNINDEXED,\
                 timestamp_ms UNINDEXED\
             );\
             -- v4:稠密向量表。message_id 关联 history 表的 rowid。\
             CREATE TABLE IF NOT EXISTS history_vectors (\
                 message_id INTEGER PRIMARY KEY,\
                 vector BLOB NOT NULL\
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            embedder: EmbeddingProviderRef::default(),
        })
```

- [ ] **Step 2: 补 import + 常量**

文件顶部补:

```rust
use std::sync::Arc;

use crate::memory_semantic::{cosine_similarity, EmbeddingProvider, EmbeddingProviderRef};
```

模块常量区(现有 `HistoryIndexError` 定义之前)新增:

```rust
/// RRF(Reciprocal Rank Fusion)融合常数,标准值 60(Cormack et al. 2009)。
const RRF_K: f64 = 60.0;
/// 超过此字符数的消息跳过稠密嵌入(巨型工具输出语义价值低且嵌入成本高);
/// 词法 FTS5 路径不受影响。
pub const MAX_EMBED_CHARS: usize = 4096;
```

- [ ] **Step 3: 写失败测试(schema v4)**

在 [history_search.rs](d:/claw-code-src/rust/crates/runtime/src/history_search.rs) 测试模块追加:

```rust
    #[test]
    fn open_creates_history_vectors_table_schema_v4() {
        let (_file, index) = open_temp_index();
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let version: i64 = conn
            .query_row(
                "SELECT value FROM history_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("schema_version row");
        assert_eq!(version, 4, "schema_version should be 4");
        let has_vec: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='history_vectors'",
                [],
                |row| row.get(0),
            )
            .expect("count history_vectors");
        assert_eq!(has_vec, 1, "history_vectors table should exist");
    }
```

- [ ] **Step 4: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::open_creates_history_vectors_table_schema_v4`
Expected: FAIL(尚无 `embedder` 字段 / version 仍为 3)。

- [ ] **Step 5: 实现(本 Task Step 1 的代码即实现)**

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::open_creates_history_vectors_table_schema_v4`
Expected: PASS。

- [ ] **Step 7: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): HistoryIndex 增加 embedder 字段 + schema v4(history_vectors 表)"
```

---

## Task 4: `index_message` 增量计算并存储向量

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs:91-113`

- [ ] **Step 1: 写失败测试**

在测试模块追加:

```rust
    #[test]
    fn index_message_stores_vector_when_embedder_injected() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        index
            .index_message("rust programming language", "s1", "user", 0, 1_000)
            .expect("index msg");
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(count, 1, "one vector row should exist");
    }

    #[test]
    fn index_message_skips_vector_without_embedder() {
        let (_file, index) = open_temp_index();
        index
            .index_message("hello world", "s1", "user", 0, 1_000)
            .expect("index msg");
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(count, 0, "no vector without embedder");
    }

    #[test]
    fn index_message_skips_embedding_oversized_content() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        let big = "x".repeat(MAX_EMBED_CHARS + 1);
        index
            .index_message(&big, "s1", "tool", 0, 1_000)
            .expect("index big msg");
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(count, 0, "oversized content should not embed");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::index_message_`
Expected: FAIL(编译错误：`with_embedder` 不存在)。

- [ ] **Step 3: 实现 `with_embedder` + 向量存储**

在 `index_message` 之前新增方法，并改造 `index_message`:

```rust
    /// 注入稠密检索 embedder(进程级共享实例,见 `crate::build_embedding_provider`)。
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn EmbeddingProvider + Send + Sync>) -> Self {
        self.embedder = EmbeddingProviderRef::new(embedder);
        self
    }

    /// Index a single message.
    ///
    /// `content` is the searchable text (typically the rendered message
    /// body). `session_id`, `role`, `message_index`, and `timestamp_ms`
    /// are stored as unindexed metadata so they can be returned with each
    /// hit without polluting the FTS5 token stream.
    ///
    /// v4:若已注入 embedder 且内容长度 ≤ [`MAX_EMBED_CHARS`],在写入词法索引
    /// 的同时增量计算向量并存入 `history_vectors`(message_id = 本行 rowid)。
    /// 向量在锁外计算,避免阻塞其他索引/检索操作;嵌入失败静默跳过(词法兜底)。
    pub fn index_message(
        &self,
        content: &str,
        session_id: &str,
        role: &str,
        message_index: usize,
        timestamp_ms: u64,
    ) -> Result<(), HistoryIndexError> {
        let vector: Option<Vec<u8>> = self.embedder.provider().and_then(|embedder| {
            if content.chars().count() > MAX_EMBED_CHARS {
                return None;
            }
            embedder.embed(content).ok().map(|v| f32_vec_to_le_bytes(&v))
        });
        let conn = self.conn.lock().expect("history index mutex poisoned");
        conn.execute(
            "INSERT INTO history (content, content_raw, session_id, role, message_index, timestamp_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                tokenize_content_for_index(content),
                content, // content_raw: 原始文本,供检索结果显示
                session_id,
                role,
                message_index as i64,
                timestamp_ms as i64,
            ],
        )?;
        if let Some(bytes) = vector {
            let rowid = conn.last_insert_rowid();
            conn.execute(
                "INSERT OR REPLACE INTO history_vectors (message_id, vector) VALUES (?1, ?2)",
                rusqlite::params![rowid, bytes],
            )?;
        }
        Ok(())
    }
```

同时在文件内(模块底部 helper 区)新增字节转换函数:

```rust
/// f32 向量 → little-endian 字节(SQLite BLOB 存储)。
fn f32_vec_to_le_bytes(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// SQLite BLOB → f32 向量。
fn f32_vec_from_le_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::index_message_`
Expected: 3 个测试全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): index_message 注入 embedder 时增量计算并存储向量"
```

---

## Task 5: `dense_search` 稠密检索

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs`(在 `search()` 之后新增方法)

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn dense_search_returns_cosine_ranked_hits() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        index
            .index_message("rust programming", "s1", "user", 0, 1_000)
            .expect("index msg 0");
        index
            .index_message("weather report today", "s1", "user", 1, 2_000)
            .expect("index msg 1");
        let hits = index
            .dense_search("rust programming", 5, &*provider)
            .expect("dense search");
        assert_eq!(hits.len(), 1, "only identical bag-of-words should pass cos>0");
        assert_eq!(hits[0].message_index, 0);
        assert!(
            (hits[0].rank - 1.0).abs() < 1e-5,
            "identical text should have cosine ~1.0, got {}",
            hits[0].rank
        );
    }
```

注意：测试需通过 `index` 对象访问 `dense_search`。若把 `dense_search` 设为私有方法，测试模块(same module)可访问——本测试直接放在 `mod tests` 内即可调用私有方法。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::dense_search_returns_cosine_ranked_hits`
Expected: FAIL(`dense_search` 未定义)。

- [ ] **Step 3: 实现 `dense_search`**

在 `search()` 方法之后、`clear_session()` 之前插入:

```rust
    /// 稠密检索:对全部已存向量做 brute-force 余弦相似度,返回 top-k。
    ///
    /// 命中 `rank` = 余弦分数(0.0-1.0,**越高越相关**)。
    /// 查询嵌入失败或向量表为空时返回空列表(由 `hybrid_search` 回退词法)。
    fn dense_search(
        &self,
        query: &str,
        top_k: usize,
        embedder: &dyn EmbeddingProvider,
    ) -> Result<Vec<HistoryHit>, HistoryIndexError> {
        let query_vec = match embedder.embed(query) {
            Ok(v) if !v.is_empty() => v,
            _ => return Ok(Vec::new()),
        };
        let conn = self.conn.lock().expect("history index mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT h.content_raw, h.session_id, h.role, h.message_index, h.timestamp_ms, v.vector \
             FROM history_vectors v \
             JOIN history h ON h.rowid = v.message_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, Vec<u8>>(5)?,
            ))
        })?;
        let mut hits: Vec<HistoryHit> = Vec::new();
        for row in rows {
            let (content, session_id, role, message_index, timestamp_ms, bytes) = row?;
            let vec = f32_vec_from_le_bytes(&bytes);
            let score = cosine_similarity(&query_vec, &vec);
            if score <= 0.0 {
                continue; // 无词袋重叠(零向量)直接跳过
            }
            hits.push(HistoryHit {
                content,
                session_id,
                role,
                message_index: message_index as usize,
                timestamp_ms,
                rank: score as f64,
            });
        }
        hits.sort_by(|a, b| {
            b.rank
                .partial_cmp(&a.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k);
        Ok(hits)
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::dense_search_returns_cosine_ranked_hits`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): dense_search 稠密向量检索(brute-force 余弦)"
```

---

## Task 6: `rrf_merge` 融合函数

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs`(新增自由函数)

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn rrf_merge_ranks_dual_hits_above_single_list_hits() {
        fn hit(session_id: &str, message_index: usize, role: &str) -> HistoryHit {
            HistoryHit {
                content: format!("{session_id}#{message_index}"),
                session_id: session_id.to_string(),
                role: role.to_string(),
                message_index,
                timestamp_ms: 0,
                rank: 0.0,
            }
        }
        let lexical = vec![hit("s", 0, "user"), hit("s", 1, "user"), hit("s", 2, "user")];
        let dense = vec![hit("s", 1, "user"), hit("s", 2, "user"), hit("s", 3, "user")];
        let merged = rrf_merge(lexical, dense, 5);
        assert_eq!(merged.len(), 4, "union of {0,1,2} and {1,2,3} = 4 distinct");
        // 双列命中(1,2)分数更高,排在最前
        assert_eq!(merged[0].message_index, 1);
        assert_eq!(merged[1].message_index, 2);
        // 单列命中(0,3)排后
        assert!(
            merged[0].rank > merged[3].rank,
            "dual-list hit must outrank single-list hit: {} vs {}",
            merged[0].rank,
            merged[3].rank
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::rrf_merge_ranks_dual_hits_above_single_list_hits`
Expected: FAIL(`rrf_merge` 未定义)。

- [ ] **Step 3: 实现 `rrf_merge` + `RRF_K` 常量**

在 `search()` 之后的自由函数区新增(若 `RRF_K` 已在 Task 3 添加则直接用):

```rust
/// RRF 融合两个按相关性排序的候选列表。
///
/// 同一逻辑消息(键 = session_id + message_index + role)出现在两列时
/// 获得双重贡献,排名显著提前 —— 词法与语义双信号一致的可信度加成。
/// 返回列表按融合分数降序,`rank` 字段写入融合分数(越高越相关)。
fn rrf_merge(lexical: Vec<HistoryHit>, dense: Vec<HistoryHit>, top_k: usize) -> Vec<HistoryHit> {
    let mut acc: std::collections::HashMap<(String, usize, String), (f64, HistoryHit)> =
        std::collections::HashMap::new();
    for (rank, hit) in lexical.into_iter().enumerate() {
        let key = (hit.session_id.clone(), hit.message_index, hit.role.clone());
        let entry = acc.entry(key).or_insert((0.0, hit));
        entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    for (rank, hit) in dense.into_iter().enumerate() {
        let key = (hit.session_id.clone(), hit.message_index, hit.role.clone());
        let entry = acc.entry(key).or_insert((0.0, hit));
        entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    let mut merged: Vec<(f64, HistoryHit)> = acc.into_values().collect();
    merged.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(top_k);
    merged
        .into_iter()
        .map(|(score, mut hit)| {
            hit.rank = score;
            hit
        })
        .collect()
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::rrf_merge_ranks_dual_hits_above_single_list_hits`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): rrf_merge 双路候选融合(双信号命中优先)"
```

---

## Task 7: `hybrid_search` 主入口

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs`(在 `search()` 之后新增方法)

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn hybrid_search_falls_back_to_lexical_without_embedder() {
        let (_file, index) = open_temp_index();
        index
            .index_message("rust toolchain guide", "s1", "user", 0, 1_000)
            .expect("index msg");
        let hits = index.hybrid_search("rust", 5).expect("hybrid search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
    }

    #[test]
    fn hybrid_search_falls_back_when_embed_fails() {
        struct FailingProvider;
        impl EmbeddingProvider for FailingProvider {
            fn embed(&self, _t: &str) -> Result<Vec<f32>, crate::memory_semantic::EmbeddingError> {
                Err(crate::memory_semantic::EmbeddingError::Inference(
                    "boom".to_string(),
                ))
            }
            fn dim(&self) -> usize {
                0
            }
            fn name(&self) -> &str {
                "failing"
            }
        }
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> = Arc::new(FailingProvider);
        let index = index.with_embedder(provider);
        index
            .index_message("rust toolchain guide", "s1", "user", 0, 1_000)
            .expect("index msg");
        let hits = index.hybrid_search("rust", 5).expect("hybrid search");
        assert!(!hits.is_empty(), "embed failure must fall back to lexical");
        assert_eq!(hits[0].session_id, "s1");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::hybrid_search_`
Expected: FAIL(`hybrid_search` 未定义)。

- [ ] **Step 3: 实现 `hybrid_search`**

在 `dense_search` 方法之后、`clear_session()` 之前插入:

```rust
    /// 混合检索:FTS5 词法 + 稠密向量双路,RRF 融合后返回 top-k。
    ///
    /// - 未注入 embedder:等价于纯词法 [`HistoryIndex::search`]。
    /// - 已注入但向量为空或查询嵌入失败:自动回退纯词法。
    /// - 返回的 `HistoryHit.rank` 为 RRF 融合分数(**越高越相关**),
    ///   与 `search` 的 BM25 rank(**越低越相关**)语义不同,勿混用。
    pub fn hybrid_search(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<HistoryHit>, HistoryIndexError> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let lexical = self.search(query, top_k.saturating_mul(2))?;
        let Some(embedder) = self.embedder.provider() else {
            return Ok(lexical);
        };
        let dense = self.dense_search(query, top_k.saturating_mul(2), embedder)?;
        if dense.is_empty() {
            return Ok(lexical);
        }
        Ok(rrf_merge(lexical, dense, top_k))
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::hybrid_search_`
Expected: 2 个测试全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): hybrid_search 词法+稠密 RRF 融合主入口(自动回退)"
```

---

## Task 8: `clear_session` 清理向量 + 迁移兼容

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs:184-199, 452-460, 505-513`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn clear_session_removes_vectors() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        index
            .index_message("msg a1", "sess-a", "user", 0, 1_000)
            .expect("index a1");
        index
            .index_message("msg a2", "sess-a", "user", 1, 2_000)
            .expect("index a2");
        index
            .index_message("msg b1", "sess-b", "user", 0, 3_000)
            .expect("index b1");
        assert_eq!(index.clear_session("sess-a").expect("clear"), 2);
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(count, 1, "only sess-b vector should remain");
        // 残留向量必须属于 sess-b
        let session: String = conn
            .query_row(
                "SELECT h.session_id FROM history_vectors v JOIN history h ON h.rowid = v.message_id",
                [],
                |row| row.get(0),
            )
            .expect("remaining vector session");
        assert_eq!(session, "sess-b");
    }

    #[test]
    fn open_migrates_v2_to_v4_keeps_searchable() {
        let file = NamedTempFile::new().expect("create temp db file");
        {
            let conn = rusqlite::Connection::open(file.path()).expect("open conn");
            conn.execute_batch(
                "CREATE TABLE history_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO history_meta VALUES ('schema_version', '2');
                 CREATE VIRTUAL TABLE history USING fts5(
                     content,
                     session_id UNINDEXED,
                     role UNINDEXED,
                     message_index UNINDEXED,
                     timestamp_ms UNINDEXED
                 );
                 INSERT INTO history VALUES ('继 续 帮 我 配 置 飞 书 ', 'sess-v2', 'user', 0, 1000);",
            )
            .expect("create v2 table");
        }
        let index = HistoryIndex::open(file.path()).expect("open migrates v2 to v4");
        let hits = index.search("飞书", 10).expect("search 飞书");
        assert_eq!(hits.len(), 1, "legacy data stays searchable");
        // v4:history_vectors 表已创建
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let has_vec: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='history_vectors'",
                [],
                |row| row.get(0),
            )
            .expect("count history_vectors");
        assert_eq!(has_vec, 1, "history_vectors should exist after migration");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::clear_session_removes_vectors`
Expected: FAIL(向量未清理，count 仍为 3)。

- [ ] **Step 3: 实现**

改造 `clear_session`:

```rust
    /// Remove all entries for a session (used on session reset / compaction).
    ///
    /// Returns the number of rows deleted.
    ///
    /// v4:同时删除该会话的稠密向量(先删向量,此时 history 行仍存在,
    /// 子查询可解析 rowid;再删词法行,避免孤立向量)。
    pub fn clear_session(&self, session_id: &str) -> Result<usize, HistoryIndexError> {
        let conn = self.conn.lock().expect("history index mutex poisoned");
        conn.execute(
            "DELETE FROM history_vectors \
             WHERE message_id IN (SELECT rowid FROM history WHERE session_id = ?1)",
            rusqlite::params![session_id],
        )?;
        let removed = conn.execute(
            "DELETE FROM history WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        Ok(removed)
    }
```

在 `migrate_from_v1`(L452-460 附近)与 `migrate_to_v3`(L505-513 附近)重建表前的 `execute_batch` 中追加 `DROP TABLE IF EXISTS history_vectors;`:

```rust
        tx.execute_batch("DROP TABLE IF EXISTS history_vectors; DROP TABLE IF EXISTS history;")?;
```

(v1/v2 无向量表，此步为防御性清理，防止 rowid 语义变化导致孤立向量。)

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::clear_session_removes_vectors`
Run: `cargo test -p runtime history_search::tests::open_migrates_v2_to_v4_keeps_searchable`
Expected: 均 PASS。

- [ ] **Step 5: 回归既有迁移/清理测试**

Run: `cargo test -p runtime history_search`
Expected: 全部 PASS(含既有 `clear_session_removes_entries`、`migration_*`)。

- [ ] **Step 6: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): clear_session 清理向量 + v1/v2 迁移重建向量表"
```

---

## Task 9: `execute_session_search` 路由 `hybrid_search` + tool spec 更新

**Files:**
- Modify: `rust/crates/runtime/src/conversation.rs:4366, 112-130`

- [ ] **Step 1: 写失败测试(注入 embedder 的混合路径)**

在 `session_search_returns_results_when_indexed` 测试之后追加:

```rust
    #[test]
    fn session_search_uses_hybrid_path_with_embedder() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_history_index();
        let provider: Arc<dyn crate::memory_semantic::EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        index
            .index_message("rust toolchain setup", "sess-h", "user", 0, 1_000)
            .expect("index msg");
        let session = Session::new().with_history_index(Arc::new(index));
        let runtime = ConversationRuntime::new(
            session,
            NoopApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        let output = runtime
            .execute_session_search(r#"{"query":"rust toolchain","top_k":5}"#)
            .expect("search should succeed");
        assert!(
            output.contains("Found 1 matches"),
            "expected hybrid match in output: {output}"
        );
        assert!(
            output.contains("session: sess-h"),
            "session id missing from output: {output}"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime conversation::tests::session_search_uses_hybrid_path_with_embedder`
Expected: 测试可 PASS 或 FAIL——关键在于实现前验证。先实现 Step 3 再运行确认行为正确(混合路径命中)。

- [ ] **Step 3: 实现路由切换**

将 [conversation.rs](d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L4366) 的:

```rust
            let hits = history_index.search(query, top_k)?;
```

替换为:

```rust
            // v4:混合检索(FTS5 词法 + 向量稠密,RRF 融合)。
            // 未注入 embedder / 嵌入失败时内部自动回退纯词法,行为与旧 search 一致。
            let hits = history_index.hybrid_search(query, top_k)?;
```

将 [conversation.rs](d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L114) 的 tool description 更新为:

```rust
    "description": "Search the conversation history using hybrid full-text + semantic search. Combines FTS5 keyword matching with dense vector recall (reciprocal rank fusion). Use this to recall specific past discussions, decisions, or file references that may not be in the current context window. Returns ranked matches with session ID, role, and content snippet. The query still supports FTS5 syntax: phrases, AND, OR, NOT, and prefix queries (term*).",
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime conversation::tests::session_search_uses_hybrid_path_with_embedder`
Run: `cargo test -p runtime conversation::tests::session_search_returns_results_when_indexed`
Expected: 均 PASS(旧测试走无 embedder 回退路径，行为不变)。

- [ ] **Step 5: 提交**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "feat(runtime): session_search 路由 hybrid_search + 更新工具描述"
```

---

## Task 10: 会话工厂注入共享 embedder

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/session_mgr.rs:1120-1123, 1140-1143`

- [ ] **Step 1: 实现注入(两个工厂)**

将 `new_cli_session` 中的:

```rust
    let db_path = cwd.join(".claw").join("history.db");
    if let Ok(index) = HistoryIndex::open(&db_path) {
        session = session.with_history_index(std::sync::Arc::new(index));
    }
```

替换为:

```rust
    let db_path = cwd.join(".claw").join("history.db");
    if let Ok(mut index) = HistoryIndex::open(&db_path) {
        // v4:注入进程级共享 embedding provider(BGE-small 单例)启用稠密检索;
        // 未编译 embedding feature 时返回 None,索引保持纯 FTS5 行为。
        if let Some(provider) = runtime::build_embedding_provider() {
            index = index.with_embedder(provider);
        }
        session = session.with_history_index(std::sync::Arc::new(index));
    }
```

`new_cli_session_with_roots` 中同样替换(两处代码块相同)。

- [ ] **Step 2: 确认 import**

确认 [session_mgr.rs](d:/claw-code-src/rust/crates/rusty-claude-cli/src/session_mgr.rs) 顶部已引入 `runtime::build_embedding_provider`(加入现有 `use runtime::{...}` 列表)。

- [ ] **Step 3: 编译验证(默认 features,含 embedding)**

Run: `cargo build -p rusty-claude-cli`
Expected: 编译成功(默认 features = full-tui + embedding + acp-0_10；首次可能因重新链接较慢)。

- [ ] **Step 4: 提交**

```bash
git add rust/crates/rusty-claude-cli/src/session_mgr.rs
git commit -m "feat(cli): 会话工厂注入共享 embedding provider 启用混合检索"
```

---

## Task 11: 全量回归与部署验证

**Files:** 无(验证)

- [ ] **Step 1: runtime 全量测试**

Run: `cargo test -p runtime`
Expected: 全量 PASS(既有 ~1740+ 测试 + 新增 ~10 个，零回归)。

- [ ] **Step 2: 无 embedding feature 构建回归**

Run: `cargo build -p runtime`
Expected: 编译通过(证明未开 embedding 时单例返回 None，纯词法路径可用)。

- [ ] **Step 3: 实测冒烟(可选,需真实 BGE 模型)**

Run: `cargo run -p rusty-claude-cli -- doctor`(或现有冒烟入口)
Expected: 启动无异常；日志出现一次 `embedding provider: fastembed (384-dim BGE-small-en-v1.5)`(进程级单例只打印一次)。

- [ ] **Step 4: 提交(如有遗留修改)**

```bash
git add -A
git commit -m "test(runtime): 混合检索回归验证"
```

---

## 已知限制与后续(不在本方案范围)

1. **历史数据不回溯嵌入**:已有 `history.db` 的消息无向量，稠密路径随新消息逐步生效。后续可选后台 lazy backfill。
2. **brute-force 余弦**:向量规模极大(>10 万条)时建议引入 ANN(如 sqlite-vec)；CLAW 工作区量级内毫秒级可接受。
3. **无 cross-encoder 重排**:True Memory 管线第 10 阶段，需新增 reranker 模型依赖，列为 Phase 2 候选。
4. **无 gzip 编码门控**:True Memory 的 novelty/salience/prediction-error gate 论文自承未验证，列为 Phase 3 候选，替代/补充现有"结论门槛"。

## Self-Review 清单

- **Spec 覆盖**:Phase 1 目标(词法+向量双路 + RRF)由 Task 3-9 完整覆盖；回退安全(Task 7)与迁移兼容(Task 8)；接线(Task 10)；回归(Task 11)。
- **占位符扫描**:无 TBD/待定；每个代码步骤含完整可编译代码。
- **类型一致性**:`EmbeddingProviderRef` 在 Task 2 定义 `new()`/`provider()`，Task 3-5/7 一致使用；`hybrid_search`/`dense_search`/`rrf_merge` 签名在 Task 5-7 间互相引用一致；`MAX_EMBED_CHARS`/`RRF_K` 在 Task 3 定义、Task 4/6 引用。
