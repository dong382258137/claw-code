# 历史检索 gzip Novelty 门控(Phase 3)实现方案

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 ingestion 期增加 gzip novelty 门控(True Memory encoding gate 的 novelty 信号)：对与已存历史高度冗余的消息跳过向量嵌入，节省 embedding 计算成本，同时**不改变** FTS5 逐字写入(保护 verbatim substrate)。

**Architecture:** 对应 True Memory 论文(arXiv:2605.04897) **Stage 1 Encoding Gate** 的 novelty 信号(论文公式)。`index_message` 在嵌入前计算 `n_t = (|gz(M ∥ e_t)| - |gz(M)|) / |gz(e_t)|`，其中 M 为"stored neighborhood"(用 FTS5 检索当前消息文本前 K=3 条已存消息拼接)。`n_t < NOVELTY_THRESHOLD(0.3)` 视为冗余 → 跳过嵌入(词法索引照常写入)。**仅 embedder 存在时启用**(gate 只影响嵌入决策，无 embedder 时本来就不嵌入)。

**Tech Stack:** Rust、flate2(gzip level 6，默认 features = miniz_oxide 纯 Rust 无系统依赖)、rusqlite 0.31(bundled FTS5)、hash-embedding 测试替身。改动在 `history_search.rs` + `Cargo.toml`。

---

## 背景与现状(代码事实)

- [history_search.rs](d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L116-L151) `index_message`：锁外 `embed()`(长度 > [`MAX_EMBED_CHARS`] 跳过)→ 持锁 INSERT 词法 + `history_vectors`。
- [history_search.rs](d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L153-L179) `search(query, top_k)`：FTS5 BM25，内部自行加锁 —— 可复用为 neighborhood 检索。
- 论文 gate 三信号：novelty(gzip 压缩成本)、salience(已 Phase 2 落地)、prediction error(预测编码，论文自承未验证且实现复杂 —— **本方案不落地**)。
- 论文明确提示 gate 会降低召回(评测禁用 gate 因为无法评分选择性摄入)—— CLAW 中 gate 只作用于**稠密嵌入决策**，词法 FTS5 全量保留不受影响，故召回损失仅限于 dense 路径对"高度冗余消息"的覆盖，可接受。

## 关键设计决策

1. **gate 只控制"是否嵌入向量"**：FTS5 verbatim 写入永远执行。低 novelty 消息失去 dense 召回能力，但词法路径不受影响(冗余消息词法也能命中)。
2. **只做 novelty 信号**：salience 已 Phase 2；prediction error 不落地(过度工程 + 论文未验证)。
3. **M = stored neighborhood**：`search(content, K=3)` 取前 3 条已存消息的 `content_raw` 拼接(插入前检索，无鸡生蛋问题)；M 总长上限 [`NOVELTY_CTX_MAX_CHARS`]=2000，控制 gzip 成本。**检索失败必须静默降级**：`neighborhood_context` 内部 `self.search(...).unwrap_or_default()`，返回 `Vec<HistoryHit>` 而非 `Result` —— 检索失败 → M 空 → 视为应嵌入(宽松方向)。**绝不让 gate 的检索错误传播为 `index_message` 的 Err**，否则边缘输入(FTS5 语法错误)会连词法写入都中止，违背"词法不受影响"承诺。
4. **公式与论文一致**：`n_t = (|gz(M∥e_t)| - |gz(M)|) / |gz(e_t)|`，gz 为 flate2 GzEncoder level 6。量级：M 与 e 完全不同 → n≈1.0；完全相同 → n≈0。
5. **阈值**：[`NOVELTY_THRESHOLD`]=0.3，`n_t < 阈值` 视为冗余跳过嵌入。恒开启、pub const 可调(与 Phase 2 一致，不接 config)。
6. **锁外计算**：neighborhood 检索(`search` 内部锁)→ 释放 → gzip 锁外 → 嵌入锁外 → 持锁写入。避免持锁做 gzip/embedding。
7. **无 embedder 时不启用**：`self.embedder.provider()` 为 None 时 gate 直接跳过(行为与 Phase 1/2 完全一致)。
8. **flate2 依赖**：`flate2 = "1"`(默认 feature rust_backend/miniz_oxide，纯 Rust 无系统 zlib 依赖)。
9. **CJK 退化方向与验证**：`search` 经 [tokenize_query_for_match](d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L247-L370) 把中文 query 拆成单字 AND，单字命中面极广，neighborhood 前 K 条可能混入"共享一个汉字但语义无关"的消息。**该退化方向是宽松的**：无关消息进 M → gzip novelty 偏高 → 仍嵌入(只是少省一点成本，**不产生"该嵌入却跳过"的漏嵌假阴性**)。Task 2 补 2 个 CJK 测试(中文重复不嵌入 / 中文迥异嵌入)验证行为不反直觉；局限在常量注释与本文档显式声明。

## 文件结构

| 文件 | 职责 | 变更类型 |
|---|---|---|
| `rust/crates/runtime/Cargo.toml` | 新增 flate2 依赖 | 修改(小) |
| `rust/crates/runtime/src/history_search.rs` | novelty 常量 + `gzip_len`/`gzip_novelty`/`neighborhood_context` + `index_message` 集成 + 测试 | 修改(核心) |

---

## Task 1: flate2 依赖 + gzip novelty 打分函数

**Files:**
- Modify: `rust/crates/runtime/Cargo.toml:8-32`(dependencies 区)
- Modify: `rust/crates/runtime/src/history_search.rs`(常量区 + 自由函数区)

- [ ] **Step 1: 加 flate2 依赖**

在 [Cargo.toml](d:/claw-code-src/rust/crates/runtime/Cargo.toml) 的 `[dependencies]` 区(`regex = "1"` 之后)新增:

```toml
# Phase 3:gzip novelty 门控(True Memory encoding gate)。纯 Rust 后端(miniz_oxide),无系统 zlib 依赖。
flate2 = "1"
```

- [ ] **Step 2: 写失败测试**

在测试模块追加:

```rust
    // -----------------------------------------------------------------
    // Phase 3:gzip novelty 门控
    // -----------------------------------------------------------------

    #[test]
    fn gzip_novelty_identical_text_is_near_zero() {
        // 与 memory 完全相同的消息:n≈0(高度冗余)
        let m = "user prefers dark mode for code review";
        let e = "user prefers dark mode for code review";
        let n = super::gzip_novelty(m, e);
        assert!(
            n < super::NOVELTY_THRESHOLD,
            "identical text should be below threshold: {n}"
        );
    }

    #[test]
    fn gzip_novelty_disparate_text_is_high() {
        let m = "user prefers dark mode for code review";
        let e = "rust async runtime tokio worker pool sizing";
        let n = super::gzip_novelty(m, e);
        assert!(
            n > 0.5,
            "disparate text should score high novelty: {n}"
        );
    }

    #[test]
    fn gzip_novelty_partial_overlap_is_mid_range() {
        let m = "rust toolchain setup with rustup on windows";
        let e = "rust toolchain configuration via rustup";
        let n = super::gzip_novelty(m, e);
        assert!(
            n >= 0.0 && n < 0.8,
            "partial overlap should land in (0, 0.8): {n}"
        );
    }
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::gzip_novelty_`
Expected: FAIL(编译错误：`gzip_novelty`/`NOVELTY_THRESHOLD` 未定义)。

- [ ] **Step 4: 实现常量 + 函数**

在模块常量区(`SALIENCE_CONTENT_BONUS_CAP` 之后)新增:

```rust
// ── Phase 3:gzip novelty 门控 ──
// 对应 True Memory encoding gate 的 novelty 信号:
// n_t = (|gz(M ∥ e_t)| - |gz(M)|) / |gz(e_t)|,gz = gzip level 6。
// n_t 低于阈值视为与已存历史高度冗余,跳过向量嵌入(词法索引不受影响)。

/// novelty 阈值:n_t < 该值视为冗余消息,跳过嵌入。
///
/// 设计假设(未经实证标定):0.3 位于"完全相同(n≈0)~ 完全不同(n≈1)"量级的中点偏保守,
/// 偏向"多嵌"(省成本为主,不牺牲召回)。中间地带(0.2–0.4)存在同主题后续消息时嵌时跳
/// 的抖动风险;若线上观察 embedding 成本收益不理想,优先在此调参(单点常量)。
pub const NOVELTY_THRESHOLD: f64 = 0.3;
/// stored neighborhood 的消息条数(search 取前 K 条已存消息拼接为 M)。
pub const NOVELTY_NEIGHBOR_K: usize = 3;
/// stored neighborhood 拼接总长上限(字符),控制 gzip 计算成本。
pub const NOVELTY_CTX_MAX_CHARS: usize = 2000;
```

在自由函数区(`salience_weight` 之后)新增:

```rust
/// gzip level 6 压缩后的字节长度(flate2 GzEncoder)。
fn gzip_len(text: &str) -> usize {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
    encoder
        .write_all(text.as_bytes())
        .expect("gzip write to vec cannot fail");
    encoder.finish().expect("gzip finish cannot fail").len()
}

/// gzip novelty 分数(True Memory encoding gate,论文公式)。
///
/// `n = (|gz(memory ∥ event)| - |gz(memory)|) / |gz(event)|`
/// - memory 与 event 完全相同 → n≈0(冗余)
/// - memory 与 event 完全不同 → n≈1(新颖)
#[must_use]
fn gzip_novelty(memory: &str, event: &str) -> f64 {
    let m_len = gzip_len(memory);
    let combined_len = gzip_len(&format!("{memory}{event}"));
    let e_len = gzip_len(event).max(1);
    (combined_len - m_len) as f64 / e_len as f64
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::gzip_novelty_`
Expected: 3 个测试全部 PASS。

- [ ] **Step 6: 提交**

```bash
git add rust/crates/runtime/Cargo.toml rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): gzip novelty 打分函数(True Memory encoding gate novelty 信号)"
```

---

## Task 2: `index_message` 集成 novelty 门控

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs`(`index_message` + 新私有方法)

- [ ] **Step 1: 写失败测试**

在测试模块追加:

```rust
    #[test]
    fn index_message_skips_embedding_for_redundant_content() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        // 第一条:唯一内容 → 嵌入
        index
            .index_message("unique rust toolchain setup content", "s1", "user", 0, 1_000)
            .expect("index first");
        // 第二条:与第一条完全相同 → novelty≈0 → 跳过嵌入
        index
            .index_message("unique rust toolchain setup content", "s1", "user", 1, 2_000)
            .expect("index duplicate");
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(
            count, 1,
            "redundant message should not create a second vector"
        );
    }

    #[test]
    fn index_message_embeds_novel_content() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        index
            .index_message("first topic about rust toolchain", "s1", "user", 0, 1_000)
            .expect("index first");
        // 内容迥异 → novelty 高 → 嵌入
        index
            .index_message("completely different weather forecast discussion", "s1", "user", 1, 2_000)
            .expect("index novel");
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(count, 2, "novel messages should both be embedded");
    }

    #[test]
    fn index_message_without_embedder_skips_gate() {
        // 无 embedder:gate 不启用,也不嵌入(与 Phase 1 行为一致)。
        let (_file, index) = open_temp_index();
        index
            .index_message("any content", "s1", "user", 0, 1_000)
            .expect("index msg");
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(count, 0);
    }

    #[test]
    fn index_message_skips_embedding_for_redundant_chinese_content() {
        // CJK 回归:中文重复消息(单字拆词,neighborhood 命中自身)→ novelty≈0 → 不重复嵌入。
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        index
            .index_message("用户偏好深色模式用于代码评审", "s1", "user", 0, 1_000)
            .expect("index chinese first");
        index
            .index_message("用户偏好深色模式用于代码评审", "s1", "user", 1, 2_000)
            .expect("index chinese duplicate");
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(
            count, 1,
            "redundant chinese message should not create a second vector"
        );
    }

    #[test]
    fn index_message_embeds_novel_chinese_content() {
        // CJK 回归:中文迥异内容(neighborhood 无相关命中或 novelty 高)→ 应嵌入。
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        index
            .index_message("配置飞书机器人 Webhook 事件订阅", "s1", "user", 0, 1_000)
            .expect("index chinese first");
        index
            .index_message("股票 K 线背驰信号量化策略复盘", "s1", "user", 1, 2_000)
            .expect("index chinese novel");
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history_vectors", [], |row| row.get(0))
            .expect("count vectors");
        assert_eq!(count, 2, "novel chinese messages should both be embedded");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::index_message_skips_embedding_for_redundant_content`
Expected: FAIL(当前实现重复消息也会嵌入，count=2)。

- [ ] **Step 3: 实现(改造 `index_message` + 新增 `neighborhood_context`)**

将 [index_message](d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L116-L151) 的向量计算段替换:

```rust
    pub fn index_message(
        &self,
        content: &str,
        session_id: &str,
        role: &str,
        message_index: usize,
        timestamp_ms: u64,
    ) -> Result<(), HistoryIndexError> {
        // Phase 3:novelty 门控 —— 决定是否嵌入(词法索引不受影响)。
        // 粗过滤(长度)+ 细过滤(gzip novelty)。仅 embedder 存在时启用。
        let mut should_embed = content.chars().count() <= MAX_EMBED_CHARS;
        if should_embed {
            if let Some(_embedder) = self.embedder.provider() {
                let memory = self.neighborhood_context(content, NOVELTY_NEIGHBOR_K);
                if !memory.is_empty() {
                    should_embed = gzip_novelty(&memory, content) >= NOVELTY_THRESHOLD;
                }
            }
        }
        // 向量在锁外计算,避免阻塞其他索引/检索操作;嵌入失败静默跳过(词法兜底)。
        let vector: Option<Vec<u8>> = if should_embed {
            self.embedder.provider().and_then(|embedder| {
                embedder.embed(content).ok().map(|v| f32_vec_to_le_bytes(&v))
            })
        } else {
            None
        };
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

在 `index_message` 之后新增私有方法:

```rust
    /// stored neighborhood:用 FTS5 检索当前消息文本,取前 `k` 条已存消息的
    /// 原始文本拼接为 gzip novelty 的 memory 上下文 M(插入前检索,不包含自身)。
    /// 拼接总长受 [`NOVELTY_CTX_MAX_CHARS`] 限制,控制 gzip 计算成本。
    ///
    /// 检索失败**静默降级为空串**(调用方视为"应嵌入"):gate 只是成本优化,
    /// 检索错误(如边缘输入的 FTS5 语法异常)绝不能传播为 `index_message` 的
    /// Err 而阻断词法写入 —— 词法 verbatim 保留不受 gate 影响。
    fn neighborhood_context(&self, content: &str, k: usize) -> String {
        let hits = self.search(content, k).unwrap_or_default(); // 失败 → 空 → 应嵌入
        let mut memory = String::new();
        for hit in hits {
            if memory.len() + hit.content.len() > NOVELTY_CTX_MAX_CHARS {
                break;
            }
            memory.push_str(&hit.content);
            memory.push('\n');
        }
        memory
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::index_message_`
Run: `cargo test -p runtime history_search`
Expected: 全部 PASS(新增 8 + 既有 33 = 41 个)。

- [ ] **Step 5: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): index_message 集成 gzip novelty 门控(冗余消息跳过嵌入)"
```

---

## Task 3: 全量回归

**Files:** 无(验证)

- [ ] **Step 1: runtime 全量测试**

Run: `cargo test -p runtime`
Expected: 全量 PASS、零回归。**测试总数以实际运行为准**(Phase 2 回归实测为 1792，Phase 3 新增 8 个后预期 ~1800；若与预期不符，以 `test result` 行实际值为准)。

- [ ] **Step 2: 无 embedding feature 构建回归**

Run: `cargo build -p runtime`
Expected: 编译通过(flate2 不依赖 embedding feature；无 embedder 时 gate 不启用)。

- [ ] **Step 3: 全工作区构建(flate2 依赖新增后确保 workspace 无冲突)**

Run: `cargo check --workspace`
Expected: 编译通过(flate2 为新增普通依赖，不影响其他 crate)。

- [ ] **Step 4: 提交(如有遗留修改)**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "test(runtime): gzip novelty 门控回归验证"
```

---

## Self-Review 清单

- **Spec 覆盖**：gzip novelty 打分(Task 1) + index_message 集成(Task 2) + 回归(Task 3)完整覆盖 Phase 3 目标。词法 verbatim 写入在 Task 2 的 INSERT 段保持不变(仅向量条件化)。
- **占位符扫描**：无 TBD；每个代码步骤含完整可编译代码。
- **类型一致性**：`gzip_novelty(memory: &str, event: &str) -> f64`、`gzip_len(text: &str) -> usize`、`neighborhood_context(&self, content: &str, k: usize) -> String`(非 Result,检索失败内部降级)在 Task 1/2 定义与调用一致；`NOVELTY_THRESHOLD`/`NOVELTY_NEIGHBOR_K`/`NOVELTY_CTX_MAX_CHARS` 在 Task 1 定义、Task 2 引用；`index_message` 签名不变，调用方零改动。
- **审查意见落实**：P0(测试用 `super::` 前缀)已修正；P1a(检索失败静默降级,不阻断词法写入)已修正；P1b(补 2 个 CJK 测试 + 退化方向显式声明)已修正；阈值 0.3 设计假设已写入常量注释。
