//! SQLite + FTS5-backed full-text history search index.
//!
//! This module provides [`HistoryIndex`], a small wrapper around a SQLite
//! virtual table (`history USING fts5`) that lets the runtime index
//! persisted conversation messages and later recall them by relevance.
//!
//! The index is intentionally decoupled from [`crate::session::Session`]:
//! `Session` holds an `Option<Arc<HistoryIndex>>` and, when present, writes
//! through to it inside `append_persisted_message`. Lookups are performed
//! via [`HistoryIndex::search`], which returns the top-`k` matches ranked
//! by FTS5's built-in BM25 rank.
//!
//! Design notes:
//! - `bundled` feature in `rusqlite` ships a static SQLite build with FTS5
//!   enabled, avoiding any system-level SQLite dependency.
//! - All columns except `content` are `UNINDEXED` so FTS5 only tokenizes
//!   the message body; metadata is carried alongside hits without bloating
//!   the search index.
//! - The connection is guarded by a `Mutex` because `rusqlite::Connection`
//!   is `!Sync` but the index is shared across threads via `Arc`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::memory_semantic::{cosine_similarity, EmbeddingProvider, EmbeddingProviderRef};

/// FTS5-backed history search index.
#[derive(Debug)]
pub struct HistoryIndex {
    conn: Mutex<Connection>,
    /// 可选的稠密检索 embedder。注入后 `index_message` 为每条消息增量
    /// 计算向量存入 `history_vectors` 表,`hybrid_search` 走词法+稠密双路。
    /// 未注入时行为与纯 FTS5 `search` 完全一致。
    embedder: EmbeddingProviderRef,
}

impl HistoryIndex {
    /// Open or create the FTS5 index at the given path.
    ///
    /// Creates the `history` virtual table if it does not already exist.
    /// The file's parent directory is created if it does not exist.
    ///
    /// Schema versioning (stored in the `history_meta` table):
    /// - v1: `history` table without `history_meta` (no CJK tokenization).
    /// - v2: `history` with `history_meta`, `content` stores CJK-tokenized
    ///   text, no `content_raw` column.
    /// - v3: adds a `content_raw` column holding the original (pre-split)
    ///   message body, so search hits can display raw text. Both v1 and v2
    ///   indexes are transparently migrated to v3 on open.
    /// - v4: adds the `history_vectors` table (message_id = history rowid)
    ///   holding dense embeddings for hybrid (lexical + vector) search.
    pub fn open(db_path: &Path) -> Result<Self, HistoryIndexError> {
        // Create parent directory (e.g. `.claw/`) if missing — prevents
        // silent failure where history_index stays None and session_search
        // becomes permanently unavailable for the session.
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut conn = Connection::open(db_path)?;
        // 版本检测与迁移:
        // - v1:有 history 表但无 history_meta(未切分 CJK)→ 重建为 v4(带 content_raw)
        // - v2:有 history_meta 且 schema_version < 3(content 已切分但无 content_raw)→ 重建为 v4
        let has_history_table = table_exists(&conn, "history");
        let has_meta = table_exists(&conn, "history_meta");
        if has_history_table && !has_meta {
            migrate_from_v1(&mut conn)?;
        } else if has_meta && current_schema_version(&conn)? < 3 {
            migrate_to_v3(&mut conn)?;
        }
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
             -- v4:稠密向量表。message_id 关联 history 表的 rowid。\n\
             CREATE TABLE IF NOT EXISTS history_vectors (\
                 message_id INTEGER PRIMARY KEY,\
                 vector BLOB NOT NULL\
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            embedder: EmbeddingProviderRef::default(),
        })
    }

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

    /// Search history with FTS5 full-text search.
    ///
    /// Returns the top-`k` results ordered by relevance (FTS5 `rank`,
    /// lower is better). The `query` string is passed verbatim to the
    /// FTS5 `MATCH` operator, so phrase queries (`"..."`), boolean
    /// operators (`AND`, `OR`, `NOT`), and prefix queries (`term*`) are
    /// all supported.
    ///
    /// §4.7.4 v3:决策点(role="decision")在 BM25 rank 基础上加权。
    /// 背景:决策推理淹没在 FTS5 噪声中,BM25 不优先决策内容。
    ///
    /// **符号约定**:SQLite FTS5 的 `rank` 列返回 BM25 分数,**越负越相关**
    /// (lower = better match,默认 `ORDER BY rank` 升序排列)。因此要让
    /// 决策点排名提前,需要让 rank 更负(绝对值更大)。
    ///
    /// **加权策略**:对 role="decision" 的命中 `rank *= 2.0`(扩大绝对值)。
    /// - 若原始 rank = -3.5(相关),加权后 = -7.0(更相关,排名提前)
    /// - 若原始 rank = -0.1(边缘匹配),加权后 = -0.2(轻微提前,不会越过强匹配)
    ///
    /// 实现策略:多取 top_k * 2 条 → 决策点 rank × 2.0 → 重新排序 → 截断到 top_k。
    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<HistoryHit>, HistoryIndexError> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().expect("history index mutex poisoned");
        // §4.7.4:多取 top_k * 2 条,为加权后截断预留空间
        let fetch_limit = (top_k * 2) as i64;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(content_raw, content), session_id, role, message_index, timestamp_ms, rank \
             FROM history \
             WHERE history MATCH ?1 \
             ORDER BY rank \
             LIMIT ?2",
        )?;
        // CJK 查询拆字 AND 连接(如 `飞书` → `(飞 AND 书)`),使 2 字中文词可命中。
        // 英文/短语/布尔运算符等非中文部分原样透传,不破坏 FTS5 语法。
        let fts_query = tokenize_query_for_match(query);
        let mut hits = stmt
            .query_map(rusqlite::params![fts_query, fetch_limit], |row| {
                Ok(HistoryHit {
                    content: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    message_index: row.get(3)?,
                    timestamp_ms: row.get(4)?,
                    rank: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        // §4.7.4:role="decision" 的命中加权(rank × 2.0)
        // FTS5 BM25 rank 越负越相关,所以 rank × 2.0 = 更负 = 排名提前
        for hit in hits.iter_mut() {
            if hit.role == "decision" {
                hit.rank *= 2.0;
            }
        }
        // 重新排序(加权后顺序可能变化)并截断到 top_k
        hits.sort_by(|a, b| {
            a.rank
                .partial_cmp(&b.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k);
        Ok(hits)
    }

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
        let mut merged = rrf_merge(lexical, dense, top_k);
        // Phase 2:salience 重加权(L3 salience reweighter)。
        // final_rank = rrf_score × salience_weight(role, content)。
        // 仅融合路径生效:无 embedder / dense 为空时直接返回词法结果,不应用。
        for hit in &mut merged {
            hit.rank *= salience_weight(&hit.role, &hit.content);
        }
        merged.sort_by(|a, b| {
            b.rank
                .partial_cmp(&a.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(merged)
    }

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

    /// Total indexed message count across all sessions.
    pub fn count(&self) -> Result<usize, HistoryIndexError> {
        let conn = self.conn.lock().expect("history index mutex poisoned");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
        Ok(count as usize)
    }
}

// ---------------------------------------------------------------------------
// CJK 分词与索引迁移辅助
// ---------------------------------------------------------------------------

// SQLite FTS5 默认 `unicode61` tokenizer 对连续 CJK 文本不切词:整串 CJK 是
// 单个 token,`飞书` 这类 2 字查询永远无法命中。以下两个函数在索引端与查询端
// 对称地做**单字切分**,使中文检索可用且不破坏英文/数字/标点检索。
//
// 索引端:连续汉字 → 每个汉字后加空格(`继续帮我配置飞书` → `继 续 帮 我 配 置 飞 书 `)。
// 查询端:连续汉字串(≥2 字) → `(字1 AND 字2 AND ...)`;短语查询(引号内) →
// `"字1 字2 ..."`(保持相邻语义)。ASCII 与 FTS5 运算符原样透传。

/// 判断字符是否属于 CJK 统一表意文字(含扩展 A-F 与兼容区)。
fn is_han(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2A6DF}'
            | '\u{2A700}'..='\u{2B73F}'
            | '\u{2B740}'..='\u{2B81F}'
            | '\u{2B820}'..='\u{2CEAF}'
    )
}

/// 索引侧切分:连续汉字之间插入空格,使 FTS5 按单字建 token。
/// 对已切分文本重复调用安全(token 集合不变,只是空白增多)。
fn tokenize_content_for_index(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    for ch in text.chars() {
        if is_han(ch) {
            out.push(ch);
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// 查询侧切分:按空白把查询拆成词,词内连续汉字串拆成单字 AND 连接
/// (短语 `"..."` 内保持空格相邻),FTS5 运算符/复杂表达式原样透传。
///
/// 注意 FTS5 语法限制:`(expr) 词`(括号表达式后直接跟 token)是语法错误,
/// 必须显式写 `AND`。因此括号表达式后若跟普通词,连接符输出 ` AND `。
fn tokenize_query_for_match(query: &str) -> String {
    /// FTS5 布尔运算符(大小写不敏感)。
    fn is_operator(part: &str) -> bool {
        matches!(
            part.to_ascii_uppercase().as_str(),
            "AND" | "OR" | "NOT"
        )
    }

    /// 把词中连续汉字拆为单字:phrase=true 用空格分隔(短语相邻语义),
    /// 否则输出 `(字1 AND 字2 ...)`。CJK 片段与 ASCII 片段之间用空格
    /// (括号表达式后跟词时显式 `AND`)连接,避免 `(规 AND 则)4` 这类
    /// 括号与裸字符粘连导致的 FTS5 语法错误(`fts5: syntax error near`).
    fn push_split(out: &mut String, word: &str, phrase: bool) {
        if word.is_empty() {
            return;
        }
        // 切成片段:连续汉字为一个 CJK 片段,连续非汉字为一个 ASCII 片段。
        let mut segments: Vec<String> = Vec::new();
        let mut buf = String::new();
        let mut buf_is_cjk = false;
        for ch in word.chars() {
            let is_cjk = is_han(ch);
            if !buf.is_empty() && is_cjk != buf_is_cjk {
                segments.push(std::mem::take(&mut buf));
            }
            buf_is_cjk = is_cjk;
            buf.push(ch);
        }
        if !buf.is_empty() {
            segments.push(buf);
        }
        let mut first = true;
        for seg in segments {
            if !first {
                // FTS5 语法限制:括号表达式后直接跟 token 必须显式写 AND。
                out.push_str(if out.ends_with(')') { " AND " } else { " " });
            }
            first = false;
            let chars: Vec<char> = seg.chars().collect();
            if !is_han(chars[0]) {
                out.push_str(&seg);
                continue;
            }
            match chars.len() {
                1 => out.push(chars[0]),
                _ if phrase => {
                    for (i, c) in chars.iter().enumerate() {
                        if i > 0 {
                            out.push(' ');
                        }
                        out.push(*c);
                    }
                }
                _ => {
                    out.push('(');
                    for (i, c) in chars.iter().enumerate() {
                        if i > 0 {
                            out.push_str(" AND ");
                        }
                        out.push(*c);
                    }
                    out.push(')');
                }
            }
        }
    }

    let mut out = String::with_capacity(query.len() + query.len() / 2);
    let mut first = true;
    let mut prev_was_operator = false;
    for part in query.split_whitespace() {
        let part_is_operator = is_operator(part);
        if !first {
            // 非运算符 part 之间统一显式 AND:词/短语/括号表达式任意两两
            // 组合都是合法 FTS5 语法,避免短语后跟括号(如
            // `"EPIC-017" (背 AND 驰)`)这类隐式连接触发 `syntax error near AND`。
            // 运算符(AND/OR/NOT)前后保持空格分隔。
            if !part_is_operator && !prev_was_operator {
                out.push_str(" AND ");
            } else {
                out.push(' ');
            }
        }
        first = false;
        if part_is_operator || part.contains(['(', ')']) {
            out.push_str(part);
            prev_was_operator = part_is_operator;
            continue;
        }
        if part.starts_with('"') && part.ends_with('"') && part.len() > 2 {
            // 短语查询:CJK 拆字空格,保持"相邻"语义
            let inner = &part[1..part.len() - 1];
            out.push('"');
            push_split(&mut out, inner, true);
            out.push('"');
            continue;
        }
        // 用户显式 FTS5 语法(前缀 `*` / 列 `:`)原样透传
        if part.contains('*') || part.contains(':') {
            out.push_str(part);
            continue;
        }
        // 纯数字 token(如 `1786293120900`)会被 FTS5 解析为列名引用,
        // 报 `no such column`。转字面短语,按普通词匹配。
        if part.chars().all(|c| c.is_ascii_digit()) {
            out.push('"');
            out.push_str(part);
            out.push('"');
            continue;
        }
        // 含 `-` 的词(如 `EPIC-017` / `n-4` / `BTC-1m`)会被 FTS5 当作
        // NOT 运算符,`-` 后的数字/词被解析成列名(`no such column: 017`)。
        // 转字面短语,`-` 作为普通字符参与匹配。
        if part.contains('-') {
            out.push('"');
            push_split(&mut out, part, true);
            out.push('"');
            continue;
        }
        push_split(&mut out, part, false);
    }
    out
}

/// 逆变换 [`tokenize_content_for_index`]:去掉"汉字后插入的空格",还原原始文本。
///
/// v2 索引的 content 列存的是切分文本(如 `继 续 帮 我 配 置 飞 书 `),迁移到 v3
/// 时用它还原显示文本。规则:
/// - 汉字后紧跟一个空格:该空格是插入的,丢弃(汉字后紧跟汉字/标点/结尾时)。
/// - 汉字后紧跟两个空格:第一个是插入的,第二个是原文的空格,保留一个。
/// - 其余字符原样保留。
fn detokenize_content(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_han(c) && i + 1 < chars.len() && chars[i + 1] == ' ' {
            if i + 2 < chars.len() && chars[i + 2] == ' ' {
                // 汉字 + 插入空格 + 原空格:保留一个空格
                out.push(c);
                out.push(' ');
                i += 3;
            } else {
                // 汉字 + 插入空格:丢弃空格
                out.push(c);
                i += 2;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

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

/// 规则式 salience 打分 —— 返回乘子(≥1.0),作用于 RRF 融合分数。
///
/// `final_rank = rrf_score × salience_weight(role, content)`。
/// 由角色基值 + 内容信号词加成组成;内容加成封顶 [`SALIENCE_CONTENT_BONUS_CAP`]。
/// 信号词匹配大小写不敏感。
#[must_use]
fn salience_weight(role: &str, content: &str) -> f64 {
    let base = match role {
        "decision" => SALIENCE_ROLE_DECISION,
        "assistant" => SALIENCE_ROLE_ASSISTANT,
        _ => SALIENCE_ROLE_BASELINE,
    };
    let lower = content.to_ascii_lowercase();
    let count_marker = |markers: &[&str], weight: f64| -> f64 {
        markers
            .iter()
            .filter(|m| lower.contains(&m.to_ascii_lowercase()))
            .count() as f64
            * weight
    };
    let bonus = count_marker(SALIENCE_STRONG_MARKERS, SALIENCE_SIGNAL_WEIGHT)
        + count_marker(SALIENCE_ERROR_MARKERS, SALIENCE_SIGNAL_WEIGHT_ERROR)
        + count_marker(SALIENCE_DECISION_MARKERS, SALIENCE_SIGNAL_WEIGHT_DECISION);
    base + bonus.min(SALIENCE_CONTENT_BONUS_CAP)
}

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

/// 读取 history_meta 中的 schema_version(表/键缺失时返回 0,触发迁移)。
///
/// 注意:history_meta.value 为 TEXT 列(SQLite 亲和性会强制把写入的数值转为
/// 文本),而 rusqlite 0.31 的 `FromSql for i64` 只接受 INTEGER 存储值,因此
/// 这里读 String 再解析,兼容 v2/v3 遗留库写入的 TEXT 版本号。
fn current_schema_version(conn: &Connection) -> Result<i64, HistoryIndexError> {
    let raw: String = conn
        .query_row(
            "SELECT value FROM history_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "0".to_string());
    Ok(raw.trim().parse::<i64>().unwrap_or(0))
}

/// 判断 SQLite 主表是否存在指定表。
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![name],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// 迁移 v1 索引(未做 CJK 切分,无 history_meta 表)到 v3:读出全部行 → DROP →
/// 重建(v3 schema,带 content_raw)→ 逐条切分重插。v1 的 content 存的是原始
/// 文本,直接回填 content_raw。
///
/// FTS5 的 `content` 列存储原始文本(tokenizer 只影响索引、不影响存储),
/// 因此旧数据可直接读出并切分后重插,历史消息一个不丢。全程事务执行。
fn migrate_from_v1(conn: &mut Connection) -> Result<(), HistoryIndexError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "SELECT content, session_id, role, message_index, timestamp_ms FROM history",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut legacy: Vec<(String, String, String, i64, i64)> = Vec::new();
        for row in rows {
            legacy.push(row?);
        }
        drop(stmt);
        tx.execute_batch("DROP TABLE IF EXISTS history_vectors; DROP TABLE IF EXISTS history;")?;
        tx.execute_batch(
            "CREATE VIRTUAL TABLE history USING fts5(\
                 content,\
                 content_raw UNINDEXED,\
                 session_id UNINDEXED,\
                 role UNINDEXED,\
                 message_index UNINDEXED,\
                 timestamp_ms UNINDEXED\
             );",
        )?;
        for (content, session_id, role, message_index, timestamp_ms) in &legacy {
            tx.execute(
                "INSERT INTO history (content, content_raw, session_id, role, message_index, timestamp_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    tokenize_content_for_index(content),
                    content, // v1 存的是原始文本,直接回填
                    session_id,
                    role,
                    message_index,
                    timestamp_ms,
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 迁移 v2 索引(schema_version=2,content 已切分、无 content_raw 列)到 v3:
/// 重建表并回填 content_raw。v2 的 content 列存的是切分文本,无法直接还原原文,
/// 用 [`detokenize_content`] 逆变换(去掉汉字后插入的空格)近似还原显示文本。
fn migrate_to_v3(conn: &mut Connection) -> Result<(), HistoryIndexError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "SELECT content, session_id, role, message_index, timestamp_ms FROM history",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut legacy: Vec<(String, String, String, i64, i64)> = Vec::new();
        for row in rows {
            legacy.push(row?);
        }
        drop(stmt);
        tx.execute_batch("DROP TABLE IF EXISTS history_vectors; DROP TABLE IF EXISTS history;")?;
        tx.execute_batch(
            "CREATE VIRTUAL TABLE history USING fts5(\
                 content,\
                 content_raw UNINDEXED,\
                 session_id UNINDEXED,\
                 role UNINDEXED,\
                 message_index UNINDEXED,\
                 timestamp_ms UNINDEXED\
             );",
        )?;
        for (content, session_id, role, message_index, timestamp_ms) in &legacy {
            tx.execute(
                "INSERT INTO history (content, content_raw, session_id, role, message_index, timestamp_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    content, // 已是切分文本,原样保留(索引 token 不变)
                    detokenize_content(content),
                    session_id,
                    role,
                    message_index,
                    timestamp_ms,
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// A single full-text search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryHit {
    /// The indexed message body.
    pub content: String,
    /// Session the message belongs to.
    pub session_id: String,
    /// Speaker role (`"user"`, `"assistant"`, `"system"`, `"tool"`).
    pub role: String,
    /// Position of the message within its session.
    pub message_index: usize,
    /// Wall-clock timestamp in milliseconds since UNIX epoch.
    pub timestamp_ms: u64,
    /// FTS5 BM25 rank (lower is more relevant). FTS5 emits `rank` as a
    /// real (double) value; rusqlite deserializes it into `f64`.
    pub rank: f64,
}

/// RRF(Reciprocal Rank Fusion)融合常数,标准值 60(Cormack et al. 2009)。
const RRF_K: f64 = 60.0;
/// 超过此字符数的消息跳过稠密嵌入(巨型工具输出语义价值低且嵌入成本高);
/// 词法 FTS5 路径不受影响。
pub const MAX_EMBED_CHARS: usize = 4096;

// ── Phase 2:salience 重加权 ──
// 对应 True Memory(L3 salience reweighter)检索期显著性加权。
// 规则式、零 LLM 成本:按角色基值 + 内容信号词累加,乘子作用于 RRF 融合分数。

/// decision 角色 salience 基值(决策点最优先;与 search() 内 decision rank×2.0 叠加,
/// 总效应 ≈×3.0,符合"决策点最高优先"意图)。
pub const SALIENCE_ROLE_DECISION: f64 = 1.5;
/// assistant 角色 salience 基值(助手陈述多含结论)。
pub const SALIENCE_ROLE_ASSISTANT: f64 = 1.2;
/// user / tool 角色 salience 基线。
pub const SALIENCE_ROLE_BASELINE: f64 = 1.0;
/// 单次内容信号词命中的加成。
pub const SALIENCE_SIGNAL_WEIGHT: f64 = 0.35;
/// 错误信号词命中加成(低于结论强标记)。
pub const SALIENCE_SIGNAL_WEIGHT_ERROR: f64 = 0.25;
/// 决策信号词命中加成(低于错误)。
pub const SALIENCE_SIGNAL_WEIGHT_DECISION: f64 = 0.2;
/// 内容信号总加成上限(防止单一消息无限膨胀)。
pub const SALIENCE_CONTENT_BONUS_CAP: f64 = 1.0;

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

/// 结论强标记词 —— 命中即视为"已确认结论",salience 最高档。
const SALIENCE_STRONG_MARKERS: &[&str] = &[
    "根因是", "原因是", "确认", "已验证", "结论", "已修复", "修复了",
    "PASS", "FAIL", "found that", "verified", "root cause",
];
/// 错误信号词 —— 工具失败/异常结果。
const SALIENCE_ERROR_MARKERS: &[&str] = &[
    "error", "panic", "fail", "failed", "报错", "失败", "异常",
];
/// 决策信号词 —— 决策/方案陈述。
const SALIENCE_DECISION_MARKERS: &[&str] = &[
    "decided", "decision", "决定", "方案", "alternatives",
];

/// Errors raised by [`HistoryIndex`] operations.
#[derive(Debug)]
pub struct HistoryIndexError {
    message: String,
}

impl HistoryIndexError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HistoryIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HistoryIndexError {}

impl From<rusqlite::Error> for HistoryIndexError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        detokenize_content, salience_weight, HistoryHit, HistoryIndex, MAX_EMBED_CHARS, rrf_merge,
    };
    use crate::memory_semantic::EmbeddingProvider;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    fn open_temp_index() -> (NamedTempFile, HistoryIndex) {
        let file = NamedTempFile::new().expect("create temp db file");
        let index = HistoryIndex::open(file.path()).expect("open history index");
        (file, index)
    }

    #[test]
    fn history_index_open_creates_fts5_table() {
        let (_file, index) = open_temp_index();
        // A fresh index should be empty.
        let count = index.count().expect("count on fresh index");
        assert_eq!(count, 0, "freshly opened index should be empty");
    }

    #[test]
    fn index_and_search_returns_relevant_results() {
        let (_file, index) = open_temp_index();

        index
            .index_message(
                "How do I configure the rust toolchain?",
                "sess-a",
                "user",
                0,
                1_000,
            )
            .expect("index msg 0");
        index
            .index_message(
                "You can use rustup to configure the rust toolchain.",
                "sess-a",
                "assistant",
                1,
                2_000,
            )
            .expect("index msg 1");
        index
            .index_message(
                "What is the weather like today?",
                "sess-b",
                "user",
                0,
                3_000,
            )
            .expect("index msg 2");

        let hits = index
            .search("rust toolchain", 10)
            .expect("search for rust toolchain");
        assert!(!hits.is_empty(), "should find rust toolchain hits");
        // Both indexed messages mentioning `rust toolchain` should match.
        assert_eq!(
            hits.len(),
            2,
            "expected exactly two hits for 'rust toolchain'"
        );
        // The user message comes first in the session; we don't assert
        // ordering between the two matches (BM25 ties on identical term
        // frequency) but both must be present.
        let contents: Vec<&str> = hits.iter().map(|h| h.content.as_str()).collect();
        assert!(
            contents
                .iter()
                .any(|c| c.contains("configure the rust toolchain")),
            "user message should be among hits: {contents:?}"
        );
        assert!(
            contents
                .iter()
                .any(|c| c.contains("rustup to configure the rust toolchain")),
            "assistant message should be among hits: {contents:?}"
        );
        // The unrelated weather message must NOT appear.
        assert!(
            !contents.iter().any(|c| c.contains("weather")),
            "weather message should not match 'rust toolchain': {contents:?}"
        );
    }

    #[test]
    fn search_with_no_matches_returns_empty() {
        let (_file, index) = open_temp_index();
        index
            .index_message("hello world", "sess-a", "user", 0, 1_000)
            .expect("index msg");

        let hits = index
            .search("nonexistentterm", 10)
            .expect("search for nonexistent term");
        assert!(hits.is_empty(), "no matches expected");
    }

    #[test]
    fn clear_session_removes_entries() {
        let (_file, index) = open_temp_index();
        index
            .index_message("message one", "sess-a", "user", 0, 1_000)
            .expect("index msg 0");
        index
            .index_message("message two", "sess-a", "assistant", 1, 2_000)
            .expect("index msg 1");
        index
            .index_message("message three", "sess-b", "user", 0, 3_000)
            .expect("index msg 2");

        assert_eq!(index.count().expect("count before clear"), 3);

        let removed = index.clear_session("sess-a").expect("clear sess-a");
        assert_eq!(removed, 2, "should remove both sess-a entries");

        assert_eq!(index.count().expect("count after clear"), 1);

        // sess-b should still be searchable.
        let hits = index.search("message", 10).expect("search after clear");
        assert_eq!(hits.len(), 1, "only sess-b message should remain");
        assert_eq!(hits[0].session_id, "sess-b");
        assert_eq!(hits[0].content, "message three");
    }

    #[test]
    fn count_returns_total_indexed() {
        let (_file, index) = open_temp_index();
        assert_eq!(index.count().expect("count 0"), 0);

        for i in 0..5 {
            index
                .index_message(
                    &format!("message {i}"),
                    "sess-a",
                    "user",
                    i,
                    1_000 + i as u64,
                )
                .expect("index msg");
        }
        assert_eq!(index.count().expect("count 5"), 5);
    }

    #[test]
    fn index_message_preserves_metadata_in_hits() {
        let (_file, index) = open_temp_index();
        index
            .index_message(
                "the quick brown fox",
                "sess-meta",
                "assistant",
                42,
                1_700_000_000_000,
            )
            .expect("index msg");

        let hits = index.search("quick", 10).expect("search quick");
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.content, "the quick brown fox");
        assert_eq!(hit.session_id, "sess-meta");
        assert_eq!(hit.role, "assistant");
        assert_eq!(hit.message_index, 42);
        assert_eq!(hit.timestamp_ms, 1_700_000_000_000);
    }

    // -----------------------------------------------------------------
    // §4.7.4 decision role 加权排序测试
    // -----------------------------------------------------------------

    #[test]
    fn search_with_top_k_zero_returns_empty() {
        let (_file, index) = open_temp_index();
        index
            .index_message("hello world", "sess", "user", 0, 1_000)
            .expect("index msg");
        let hits = index.search("hello", 0).expect("search with top_k=0");
        assert!(hits.is_empty(), "top_k=0 should return empty");
    }

    #[test]
    fn decision_role_gets_rank_boosted() {
        // 相同内容、不同 role:decision 的 rank 应被 × 0.5,排名提前
        let (_file, index) = open_temp_index();
        // 普通用户消息
        index
            .index_message(
                "decided to use rust toolchain for the project",
                "sess-a",
                "user",
                0,
                1_000,
            )
            .expect("index user msg");
        // 决策点消息(相同内容)
        index
            .index_message(
                "decided to use rust toolchain for the project",
                "sess-a",
                "decision",
                0,
                2_000,
            )
            .expect("index decision msg");

        let hits = index
            .search("rust toolchain", 10)
            .expect("search rust toolchain");
        assert_eq!(hits.len(), 2, "both messages should match");
        // decision 应该排第一(rank 更负 = 更相关)
        assert_eq!(
            hits[0].role, "decision",
            "decision role should rank first due to × 2.0 boost (more negative rank)"
        );
        assert_eq!(hits[1].role, "user");
        // 验证 decision 的 rank 确实更负(更相关)
        assert!(
            hits[0].rank < hits[1].rank,
            "decision rank ({}) should be < user rank ({}) [more negative = better]",
            hits[0].rank,
            hits[1].rank
        );
    }

    #[test]
    fn decision_role_boost_fits_within_top_k() {
        // top_k=1 时,如果 decision 和 user 都匹配,decision 应该占唯一名额
        let (_file, index) = open_temp_index();
        index
            .index_message(
                "use rust toolchain configuration guide",
                "sess-a",
                "user",
                0,
                1_000,
            )
            .expect("index user msg");
        index
            .index_message(
                "decided to use rust toolchain for build",
                "sess-a",
                "decision",
                0,
                2_000,
            )
            .expect("index decision msg");

        let hits = index.search("rust toolchain", 1).expect("search top_k=1");
        assert_eq!(hits.len(), 1, "top_k=1 should return exactly 1 hit");
        // 由于多取 top_k*2=2 条,加权后 decision 应该胜出
        assert_eq!(
            hits[0].role, "decision",
            "decision should win the single slot due to rank boost"
        );
    }

    #[test]
    fn non_decision_roles_are_not_boosted() {
        // 验证只有 role="decision" 被加权,其他 role(user/assistant/tool/system)不受影响
        let (_file, index) = open_temp_index();
        index
            .index_message("use rust toolchain", "s", "user", 0, 1)
            .expect("index");
        index
            .index_message("use rust toolchain", "s", "assistant", 0, 2)
            .expect("index");
        index
            .index_message("use rust toolchain", "s", "tool", 0, 3)
            .expect("index");
        index
            .index_message("use rust toolchain", "s", "system", 0, 4)
            .expect("index");

        let hits = index.search("rust toolchain", 10).expect("search");
        assert_eq!(hits.len(), 4, "all 4 should match");
        // 没有 decision role,所有 rank 应保持原样(BM25 原始排序)
        // 验证没有 hit 的 rank 被异常减半(通过检查 rank 单调递增)
        for window in hits.windows(2) {
            assert!(
                window[0].rank <= window[1].rank,
                "non-decision hits should remain in BM25 order: {} vs {}",
                window[0].rank,
                window[1].rank
            );
        }
    }

    // -----------------------------------------------------------------
    // CJK 中文检索(§修复:Bug-1 中文查询失效)
    // -----------------------------------------------------------------

    #[test]
    fn search_chinese_query_finds_matches() {
        let (_file, index) = open_temp_index();
        index
            .index_message("如何配置飞书机器人 Webhook", "sess-a", "user", 0, 1)
            .expect("index msg");
        index
            .index_message("今天天气如何", "sess-b", "user", 0, 2)
            .expect("index msg");

        // 2 字中文词(unicode61 下完全无法命中)现在可搜
        let hits = index.search("飞书", 10).expect("search 飞书");
        assert_eq!(hits.len(), 1, "2-char CJK query should hit");
        assert_eq!(hits[0].session_id, "sess-a");

        // 多字词
        let hits = index.search("机器人", 10).expect("search 机器人");
        assert_eq!(hits.len(), 1);

        // 混合中英文查询
        let hits = index.search("飞书 Webhook", 10).expect("search mixed");
        assert_eq!(hits.len(), 1);

        // 中文短语查询(相邻语义)
        let hits = index.search("\"配置飞书\"", 10).expect("search phrase");
        assert_eq!(hits.len(), 1);

        // 无关查询命中正确会话
        let hits = index.search("天气", 10).expect("search 天气");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess-b");
    }

    #[test]
    fn index_message_keeps_raw_content_for_cjk() {
        // 索引 CJK 消息后,检索命中的 content 必须是原始文本(不含切分空格)
        let (_file, index) = open_temp_index();
        let raw = "如何配置飞书机器人 Webhook";
        index
            .index_message(raw, "sess-a", "user", 0, 1_000)
            .expect("index msg");
        let hits = index.search("飞书", 10).expect("search 飞书");
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].content, raw,
            "hit.content 必须是原始文本,而不是切分后的 '如 何 配 置 飞 书 ...'"
        );
    }

    #[test]
    fn tokenize_query_preserves_fts_syntax() {
        // 非中文查询:part 间统一显式 AND(词/短语任意组合均合法)
        assert_eq!(
            super::tokenize_query_for_match("rust toolchain"),
            "rust AND toolchain"
        );
        // 中英文混合:中文拆字 AND,词之间显式 AND
        assert_eq!(
            super::tokenize_query_for_match("飞书 Webhook"),
            "(飞 AND 书) AND Webhook"
        );
        // 短语内中文拆字空格(保持相邻语义)
        assert_eq!(
            super::tokenize_query_for_match("\"配置飞书\""),
            "\"配 置 飞 书\""
        );
        // 布尔运算符保留(operator 之间仍为空格)
        assert_eq!(
            super::tokenize_query_for_match("飞书 OR feishu"),
            "(飞 AND 书) OR feishu"
        );
        // 含 `-` 的词(EPIC-017 / n-4)转字面短语,避免被 FTS5 解析为
        // NOT 运算符导致 `no such column: 017`(会话实测失败场景)
        assert_eq!(
            super::tokenize_query_for_match("EPIC-017 背驰 恢复"),
            "\"EPIC-017\" AND (背 AND 驰) AND (恢 AND 复)"
        );
        assert_eq!(super::tokenize_query_for_match("n-4"), "\"n-4\"");
        // 纯数字 token 转字面短语,避免 `no such column` 列名解析
        assert_eq!(
            super::tokenize_query_for_match("1786293120900"),
            "\"1786293120900\""
        );
        // CJK 与 ASCII 混合 token:片段间显式 AND,避免 `(规 AND 则)4` 粘连语法错误
        assert_eq!(super::tokenize_query_for_match("规则4"), "(规 AND 则) AND 4");
        // 索引切分:连续汉字间插空格,英文不受影响
        assert_eq!(
            super::tokenize_content_for_index("如何配置飞书 Feishu"),
            "如 何 配 置 飞 书  Feishu"
        );
    }

    #[test]
    fn migration_reindexes_legacy_cjk_content() {
        // 构造 v1 索引(未切分 content,无 history_meta 表)
        let file = NamedTempFile::new().expect("create temp db file");
        {
            let conn = rusqlite::Connection::open(file.path()).expect("open conn");
            conn.execute_batch(
                "CREATE VIRTUAL TABLE history USING fts5(
                    content,
                    session_id UNINDEXED,
                    role UNINDEXED,
                    message_index UNINDEXED,
                    timestamp_ms UNINDEXED
                );
                INSERT INTO history VALUES ('继续帮我配置飞书机器人', 'sess-legacy', 'user', 0, 1000);
                INSERT INTO history VALUES ('the quick brown fox', 'sess-legacy', 'assistant', 1, 2000);",
            )
            .expect("create v1 table");
        }

        // 首次 open 触发迁移
        let index = HistoryIndex::open(file.path()).expect("open migrates v1");
        let hits = index.search("飞书", 10).expect("search 飞书 after migration");
        assert_eq!(
            hits.len(),
            1,
            "legacy CJK content should be searchable after migration"
        );
        assert_eq!(hits[0].session_id, "sess-legacy");
        // v1 存的是原始文本,迁移后 content_raw 直接回填原文
        assert_eq!(hits[0].content, "继续帮我配置飞书机器人");
        // 英文旧数据同样可搜
        let hits = index
            .search("quick brown", 10)
            .expect("search english after migration");
        assert_eq!(hits.len(), 1);

        // 二次 open 不重复迁移(幂等,count 不变)
        let index2 = HistoryIndex::open(file.path()).expect("open again");
        assert_eq!(index2.count().expect("count after second open"), 2);
        let hits2 = index2.search("飞书", 10).expect("search 飞书 after second open");
        assert_eq!(hits2.len(), 1);
    }

    #[test]
    fn migration_v2_backfills_content_raw() {
        // 构造 v2 索引:history_meta 存在且 schema_version=2,content 已切分,无 content_raw 列
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

        let index = HistoryIndex::open(file.path()).expect("open migrates v2 to v3");
        let hits = index.search("飞书", 10).expect("search 飞书 after v3 migration");
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].content, "继续帮我配置飞书",
            "content_raw 必须由切分文本逆变换还原"
        );
        // 二次 open 不重复迁移(幂等)
        let index2 = HistoryIndex::open(file.path()).expect("open again");
        assert_eq!(index2.count().expect("count after second open"), 1);
    }

    #[test]
    fn detokenize_content_reconstructs_raw_text() {
        // 纯汉字串
        assert_eq!(detokenize_content("继 续 帮 我 配 置 飞 书 "), "继续帮我配置飞书");
        // 汉字 + 原文空格
        assert_eq!(detokenize_content("飞 书  配 置 "), "飞书 配置");
        // 汉字 + 英文(无空格)
        assert_eq!(detokenize_content("飞 书 Feishu"), "飞书Feishu");
        // 汉字 + 原文空格 + 英文
        assert_eq!(detokenize_content("飞 书  Feishu"), "飞书 Feishu");
        // 无汉字:原样
        assert_eq!(detokenize_content("the quick brown fox"), "the quick brown fox");
        // 汉字 + 标点
        assert_eq!(detokenize_content("配 置 完 成 。"), "配置完成。");
    }

    // -----------------------------------------------------------------
    // v4:history_vectors 表 + schema_version=4
    // -----------------------------------------------------------------

    #[test]
    fn open_creates_history_vectors_table_schema_v4() {
        let (_file, index) = open_temp_index();
        let conn = index.conn.lock().unwrap_or_else(|e| e.into_inner());
        // history_meta.value 是 TEXT 列,读 String 再解析(断言 schema_version 语义为 4)。
        let raw: String = conn
            .query_row(
                "SELECT value FROM history_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("schema_version row");
        let version: i64 = raw.parse().expect("schema_version numeric");
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

    #[test]
    fn dense_search_returns_cosine_ranked_hits() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider.clone());
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
        assert_eq!(merged.len(), 4, "union of {{0,1,2}} and {{1,2,3}} = 4 distinct");
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

    // -----------------------------------------------------------------
    // Phase 2:salience 重加权
    // -----------------------------------------------------------------

    #[test]
    fn salience_weight_role_base_ordering() {
        // 决策点 > 助手 > 用户/工具
        let decision = salience_weight("decision", "plain text");
        let assistant = salience_weight("assistant", "plain text");
        let user = salience_weight("user", "plain text");
        let tool = salience_weight("tool", "plain text");
        assert!(decision > assistant, "decision should outrank assistant");
        assert!(assistant > user, "assistant should outrank user");
        assert_eq!(user, tool, "user and tool share the baseline");
        assert_eq!(user, 1.0, "user baseline should be 1.0");
    }

    #[test]
    fn salience_weight_content_signals_add_up() {
        // 结论强标记提升 assistant 陈述
        let with_conclusion = salience_weight("assistant", "根因是缓存失效,已修复,测试 PASS");
        let plain = salience_weight("assistant", "plain text without signals");
        assert!(
            with_conclusion > plain,
            "conclusion signals should raise salience: {} vs {}",
            with_conclusion,
            plain
        );
        // 错误信号提升 tool 结果
        let with_error = salience_weight("tool", "command failed with panic: timeout");
        let tool_plain = salience_weight("tool", "completed");
        assert!(with_error > tool_plain, "error signals should raise salience");
        // 决策信号提升 user 消息
        let with_decision = salience_weight("user", "decided to use rust toolchain");
        assert!(with_decision > 1.0, "decision signal should raise salience");
    }

    #[test]
    fn salience_weight_caps_content_bonus() {
        // 多个信号词命中,内容加成封顶 +1.0
        let mut text = String::new();
        for _ in 0..10 {
            text.push_str("根因是 confirmed verified PASS ");
        }
        let score = salience_weight("user", &text);
        assert!(
            score <= 2.0,
            "content bonus should cap at +1.0, got {}",
            score
        );
    }

    #[test]
    fn salience_weight_case_insensitive() {
        let upper = salience_weight("tool", "PANIC: VERIFIED FAIL");
        let lower = salience_weight("tool", "panic: verified fail");
        assert_eq!(upper, lower, "signals should be case-insensitive");
    }

    #[test]
    fn hybrid_search_applies_salience_boost_to_decision() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider.clone());
        // user 消息词法(短文档 BM25 更高)+ 稠密(余弦 1.0)双路都领先;
        // decision 词法靠 search() 内部 ×2.0 领先、稠密被 filler 挤出前二。
        // 未集成时 user 融合分更高(确定性,非并列);salience 层 ×1.5 必须反超。
        index
            .index_message("rust toolchain", "s1", "user", 0, 1_000)
            .expect("index user");
        index
            .index_message("rust toolchain build cargo rustup project", "s1", "decision", 1, 2_000)
            .expect("index decision");
        // 第三条消息仅稠密命中(词法缺 toolchain),把 decision 挤到 dense rank 2
        index
            .index_message("rust", "s1", "user", 2, 3_000)
            .expect("index filler");
        let hits = index.hybrid_search("rust toolchain", 2).expect("hybrid");
        assert_eq!(hits.len(), 2, "top_k=2 should return two hits");
        let decision_pos = hits
            .iter()
            .position(|h| h.role == "decision")
            .expect("decision hit present");
        let user_pos = hits.iter().position(|h| h.role == "user").expect("user hit present");
        assert!(
            decision_pos < user_pos,
            "decision should rank above user after salience reweight: {decision_pos} vs {user_pos}"
        );
    }

    #[test]
    fn hybrid_search_boosts_conclusion_heavy_assistant_message() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        // 两条 assistant 消息都命中词法查询,但一条含结论强标记
        index
            .index_message("rust toolchain setup complete", "s1", "assistant", 0, 1_000)
            .expect("index plain");
        index
            .index_message("rust toolchain root cause verified, PASS", "s1", "assistant", 1, 2_000)
            .expect("index conclusion");
        let hits = index.hybrid_search("rust toolchain", 5).expect("hybrid");
        assert_eq!(hits.len(), 2);
        let conclusion_pos = hits
            .iter()
            .position(|h| h.message_index == 1)
            .expect("conclusion hit");
        let plain_pos = hits
            .iter()
            .position(|h| h.message_index == 0)
            .expect("plain hit");
        assert!(
            conclusion_pos < plain_pos,
            "conclusion-heavy message should rank above plain: {conclusion_pos} vs {plain_pos}"
        );
    }

    #[test]
    fn hybrid_search_without_embedder_skips_salience() {
        // 无 embedder:hybrid_search 直接返回词法结果,不应用 salience。
        let (_file, index) = open_temp_index();
        index
            .index_message("rust toolchain decision", "s1", "decision", 0, 1_000)
            .expect("index");
        let hits = index.hybrid_search("rust", 5).expect("hybrid");
        assert!(!hits.is_empty(), "lexical fallback should still work");
    }

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
        // 注意:短文本下 gzip 头尾固定开销(≈18B)抬高分母,迥异但过短的文本
        // n 只能到 ~0.36;因此用较长的迥异文本验证"高 novelty"语义。
        let m = "user prefers dark mode for code review sessions because it reduces eye strain during long working hours";
        let e = "rust async runtime tokio worker pool sizing strategy for high concurrency web services with graceful shutdown";
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
}
