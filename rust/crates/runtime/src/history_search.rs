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
use std::sync::Mutex;

use rusqlite::Connection;

/// FTS5-backed history search index.
#[derive(Debug)]
pub struct HistoryIndex {
    conn: Mutex<Connection>,
}

impl HistoryIndex {
    /// Open or create the FTS5 index at the given path.
    ///
    /// Creates the `history` virtual table if it does not already exist.
    /// The file's parent directory is created if it does not exist.
    pub fn open(db_path: &Path) -> Result<Self, HistoryIndexError> {
        // Create parent directory (e.g. `.claw/`) if missing — prevents
        // silent failure where history_index stays None and session_search
        // becomes permanently unavailable for the session.
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS history USING fts5(
                content,
                session_id UNINDEXED,
                role UNINDEXED,
                message_index UNINDEXED,
                timestamp_ms UNINDEXED
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Index a single message.
    ///
    /// `content` is the searchable text (typically the rendered message
    /// body). `session_id`, `role`, `message_index`, and `timestamp_ms`
    /// are stored as unindexed metadata so they can be returned with each
    /// hit without polluting the FTS5 token stream.
    pub fn index_message(
        &self,
        content: &str,
        session_id: &str,
        role: &str,
        message_index: usize,
        timestamp_ms: u64,
    ) -> Result<(), HistoryIndexError> {
        let conn = self.conn.lock().expect("history index mutex poisoned");
        conn.execute(
            "INSERT INTO history (content, session_id, role, message_index, timestamp_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                content,
                session_id,
                role,
                message_index as i64,
                timestamp_ms as i64,
            ],
        )?;
        Ok(())
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
            "SELECT content, session_id, role, message_index, timestamp_ms, rank \
             FROM history \
             WHERE history MATCH ?1 \
             ORDER BY rank \
             LIMIT ?2",
        )?;
        let mut hits = stmt
            .query_map(rusqlite::params![query, fetch_limit], |row| {
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

    /// Remove all entries for a session (used on session reset / compaction).
    ///
    /// Returns the number of rows deleted.
    pub fn clear_session(&self, session_id: &str) -> Result<usize, HistoryIndexError> {
        let conn = self.conn.lock().expect("history index mutex poisoned");
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
    use super::HistoryIndex;
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
}
