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
    /// The file's parent directory must already exist.
    pub fn open(db_path: &Path) -> Result<Self, HistoryIndexError> {
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
    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<HistoryHit>, HistoryIndexError> {
        let conn = self.conn.lock().expect("history index mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT content, session_id, role, message_index, timestamp_ms, rank \
             FROM history \
             WHERE history MATCH ?1 \
             ORDER BY rank \
             LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(rusqlite::params![query, top_k as i64], |row| {
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
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
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
            contents.iter().any(|c| c.contains("configure the rust toolchain")),
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

        let hits = index
            .search("quick", 10)
            .expect("search quick");
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.content, "the quick brown fox");
        assert_eq!(hit.session_id, "sess-meta");
        assert_eq!(hit.role, "assistant");
        assert_eq!(hit.message_index, 42);
        assert_eq!(hit.timestamp_ms, 1_700_000_000_000);
    }
}
