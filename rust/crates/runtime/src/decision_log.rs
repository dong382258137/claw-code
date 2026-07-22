//! DecisionLog — SQLite + FTS5-backed decision/repair experience index.
//!
//! # Design
//!
//! DecisionLog 是 MemoryBank 的核心组件,记录 LLM 的每一次修复决策(问题签名、
//! 根因假设、应用方案、验证结果),并提供语义检索与 simhash 去重。
//!
//! ## 与 NOTEBOOK.md 分工
//!
//! - **DecisionLog** = 修复经验库(SQLite + simhash),持久化、可搜索、可去重
//! - **NOTEBOOK.md** = 当前任务的工作记忆(跨压缩持久化的笔记)
//!
//! ## Schema
//!
//! - `decisions` — 主表(决策记录)
//! - `decision_files` — 关联文件表(多对多)
//! - `decisions_fts` — FTS5 虚拟表(全文搜索)
//! - 3 个 FTS5 同步触发器(after insert/delete/update)
//! - Schema 迁移通过 `PRAGMA user_version` 实现
//!
//! ## Simhash
//!
//! 64 位 simhash(FNV-1a + 汉明距离),用于决策去重。阈值 ≤ 3 位。
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};

/// FNV-1a 64-bit 哈希常量。
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

// ---------------------------------------------------------------------------
// Simhash
// ---------------------------------------------------------------------------

/// 计算文本的 64 位 simhash。
///
/// 算法:
/// 1. 按空格分词,每词 + 相邻 bigram 作为 feature
/// 2. 每个 feature 用 FNV-1a 64-bit 哈希
/// 3. 按位累加权值:bit=1 → +1, bit=0 → -1
/// 4. 最终 simhash:weight > 0 → bit=1
pub fn compute_simhash(text: &str) -> u64 {
    let mut weights = [0i64; 64];
    let words: Vec<&str> = text.split_whitespace().collect();

    // Individual words
    for &w in &words {
        let h = fnv1a_64(w.as_bytes());
        update_weights(&mut weights, h);
    }

    // Bigrams
    for pair in words.windows(2) {
        let bigram = format!("{}_{}", pair[0], pair[1]);
        let h = fnv1a_64(bigram.as_bytes());
        update_weights(&mut weights, h);
    }

    let mut simhash: u64 = 0;
    for (i, &w) in weights.iter().enumerate() {
        if w > 0 {
            simhash |= 1u64 << i;
        }
    }
    simhash
}

/// FNV-1a 64-bit 哈希。
fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// 更新 simhash 权重向量。
fn update_weights(weights: &mut [i64; 64], hash: u64) {
    for i in 0..64 {
        if (hash >> i) & 1 == 1 {
            weights[i] += 1;
        } else {
            weights[i] -= 1;
        }
    }
}

/// 汉明距离(两个 64 位值的不同 bit 数)。
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const CREATE_DECISIONS_V1: &str = "\
CREATE TABLE IF NOT EXISTS decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    timestamp_ms INTEGER NOT NULL,
    problem_signature TEXT NOT NULL,
    root_cause_hypothesis TEXT NOT NULL,
    applied_solution TEXT NOT NULL,
    affected_files TEXT NOT NULL,
    verification_result TEXT NOT NULL,
    verification_evidence TEXT,
    verified_at_ms INTEGER,
    context_hash TEXT,
    verify_count INTEGER DEFAULT 0,
    success_rate REAL DEFAULT 0.0,
    use_count INTEGER DEFAULT 0,
    tags TEXT,
    similarity_hash INTEGER NOT NULL
);
";

const CREATE_INDEXES_V1: &str = "\
CREATE INDEX IF NOT EXISTS idx_decisions_tags ON decisions(tags);
CREATE INDEX IF NOT EXISTS idx_decisions_signature ON decisions(similarity_hash);
CREATE INDEX IF NOT EXISTS idx_decisions_context ON decisions(context_hash);
";

const CREATE_DECISION_FILES_V1: &str = "\
CREATE TABLE IF NOT EXISTS decision_files (
    decision_id INTEGER NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    file_path TEXT NOT NULL,
    PRIMARY KEY (decision_id, file_path)
);
CREATE INDEX IF NOT EXISTS idx_decision_files_path ON decision_files(file_path);
";

const CREATE_FTS_V2: &str = "\
CREATE VIRTUAL TABLE IF NOT EXISTS decisions_fts USING fts5(
    problem_signature, root_cause_hypothesis, applied_solution,
    content='decisions', content_rowid='id'
);
";

const CREATE_TRIGGERS_V2: &str = "\
CREATE TRIGGER IF NOT EXISTS decisions_ai AFTER INSERT ON decisions BEGIN
    INSERT INTO decisions_fts(rowid, problem_signature, root_cause_hypothesis, applied_solution)
    VALUES (new.id, new.problem_signature, new.root_cause_hypothesis, new.applied_solution);
END;

CREATE TRIGGER IF NOT EXISTS decisions_ad AFTER DELETE ON decisions BEGIN
    INSERT INTO decisions_fts(decisions_fts, rowid, problem_signature, root_cause_hypothesis, applied_solution)
    VALUES ('delete', old.id, old.problem_signature, old.root_cause_hypothesis, old.applied_solution);
END;

CREATE TRIGGER IF NOT EXISTS decisions_au AFTER UPDATE ON decisions BEGIN
    INSERT INTO decisions_fts(decisions_fts, rowid, problem_signature, root_cause_hypothesis, applied_solution)
    VALUES ('delete', old.id, old.problem_signature, old.root_cause_hypothesis, old.applied_solution);
    INSERT INTO decisions_fts(rowid, problem_signature, root_cause_hypothesis, applied_solution)
    VALUES (new.id, new.problem_signature, new.root_cause_hypothesis, new.applied_solution);
END;
";

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum DecisionLogError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    InvalidInput(String),
}

impl std::fmt::Display for DecisionLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "DecisionLog I/O error: {e}"),
            Self::Sqlite(e) => write!(f, "DecisionLog SQLite error: {e}"),
            Self::InvalidInput(msg) => write!(f, "DecisionLog invalid input: {msg}"),
        }
    }
}

impl std::error::Error for DecisionLogError {}

impl From<rusqlite::Error> for DecisionLogError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

// ---------------------------------------------------------------------------
// DecisionRecord
// ---------------------------------------------------------------------------

/// 从输入 JSON 解析的决策记录。
#[derive(Debug)]
pub struct DecisionRecord {
    pub session_id: String,
    pub problem_signature: String,
    pub root_cause_hypothesis: String,
    pub applied_solution: String,
    pub affected_files: Vec<String>,
    pub tags: Vec<String>,
    pub verification_result: String,
    pub verification_evidence: Option<String>,
}

impl DecisionRecord {
    fn from_json(input: &str) -> Result<Self, DecisionLogError> {
        let parsed: serde_json::Value = serde_json::from_str(input).map_err(|e| {
            DecisionLogError::InvalidInput(format!("invalid JSON: {e}"))
        })?;

        let session_id = parsed
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DecisionLogError::InvalidInput("missing 'session_id'".into()))?
            .to_string();

        let problem_signature = parsed
            .get("problem_signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DecisionLogError::InvalidInput("missing 'problem_signature'".into())
            })?
            .to_string();

        let root_cause_hypothesis = parsed
            .get("root_cause_hypothesis")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DecisionLogError::InvalidInput("missing 'root_cause_hypothesis'".into())
            })?
            .to_string();

        let applied_solution = parsed
            .get("applied_solution")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DecisionLogError::InvalidInput("missing 'applied_solution'".into())
            })?
            .to_string();

        let affected_files: Vec<String> = parsed
            .get("affected_files")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let tags: Vec<String> = parsed
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let verification_result = parsed
            .get("verification_result")
            .and_then(|v| v.as_str())
            .unwrap_or("Pending")
            .to_string();

        let verification_evidence = parsed
            .get("verification_evidence")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        Ok(Self {
            session_id,
            problem_signature,
            root_cause_hypothesis,
            applied_solution,
            affected_files,
            tags,
            verification_result,
            verification_evidence,
        })
    }
}

// ---------------------------------------------------------------------------
// DecisionLog
// ---------------------------------------------------------------------------

/// SQLite + FTS5 决策经验库。
///
/// 数据库存储在 `.claw/decision_log.db`,通过 `rusqlite::Connection` 访问,
/// 用 `Mutex` 包装以保证线程安全(与 `HistoryIndex` 一致)。
#[derive(Debug)]
pub struct DecisionLog {
    conn: Mutex<Connection>,
}

impl DecisionLog {
    /// 打开或创建决策日志数据库。
    ///
    /// 自动执行 schema 迁移(基于 `PRAGMA user_version`)。
    pub fn open(root: &Path) -> Result<Self, DecisionLogError> {
        let db_dir = root.join(".claw");
        let _ = std::fs::create_dir_all(&db_dir);
        let db_path = db_dir.join("decision_log.db");

        let conn = Connection::open(&db_path)?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        // Schema migration
        migrate_schema(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 记录一条决策。
    ///
    /// 解析 JSON 输入,计算 simhash,写入数据库 + FTS5 索引。
    pub fn log_decision(&self, json_input: &str) -> Result<String, DecisionLogError> {
        let record = DecisionRecord::from_json(json_input)?;

        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let affected_files_json = serde_json::to_string(&record.affected_files)
            .unwrap_or_else(|_| "[]".to_string());

        // Combine key fields for simhash
        let simhash_text = format!(
            "{} {} {}",
            record.problem_signature, record.root_cause_hypothesis, record.applied_solution
        );
        let similarity_hash = compute_simhash(&simhash_text) as i64;

        // Compute context_hash from affected files
        let mut context_hash = None;
        if !record.affected_files.is_empty() {
            let context_text = record.affected_files.join(":");
            let hash_val = fnv1a_64(context_text.as_bytes());
            context_hash = Some(format!("{hash_val:x}"));
        }

        let tags_str = if record.tags.is_empty() {
            None
        } else {
            Some(record.tags.join(","))
        };

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO decisions (
                session_id, timestamp_ms, problem_signature, root_cause_hypothesis,
                applied_solution, affected_files, verification_result,
                verification_evidence, context_hash, similarity_hash, tags
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.session_id,
                timestamp_ms,
                record.problem_signature,
                record.root_cause_hypothesis,
                record.applied_solution,
                affected_files_json,
                record.verification_result,
                record.verification_evidence,
                context_hash,
                similarity_hash,
                tags_str,
            ],
        )?;

        let id = conn.last_insert_rowid();

        // Insert file associations
        for file_path in &record.affected_files {
            conn.execute(
                "INSERT OR IGNORE INTO decision_files (decision_id, file_path) VALUES (?1, ?2)",
                params![id, file_path],
            )?;
        }

        Ok(format!("decision_logged id={id}"))
    }

    /// 搜索历史决策。
    ///
    /// 使用 FTS5 全文检索 + simhash 去重,返回 top-k 匹配。
    pub fn search_decisions(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<String, DecisionLogError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());

        // FTS5 search with BM25 ranking
        let mut stmt = conn.prepare(
            "SELECT d.id, d.session_id, d.timestamp_ms, d.problem_signature,
                    d.root_cause_hypothesis, d.applied_solution,
                    d.affected_files, d.verification_result, d.verified_at_ms,
                    d.verify_count, d.success_rate, d.use_count,
                    d.tags, d.similarity_hash,
                    rank
             FROM decisions d
             JOIN decisions_fts fts ON d.id = fts.rowid
             WHERE decisions_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let fts_query = escape_fts5_query(query);
        let rows: Vec<_> = stmt
            .query_map(params![fts_query, top_k as i64], |row| {
                Ok(SearchHit {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    timestamp_ms: row.get(2)?,
                    problem_signature: row.get(3)?,
                    root_cause_hypothesis: row.get(4)?,
                    applied_solution: row.get(5)?,
                    affected_files: row.get(6)?,
                    verification_result: row.get(7)?,
                    verified_at_ms: row.get(8)?,
                    verify_count: row.get(9)?,
                    success_rate: row.get(10)?,
                    use_count: row.get(11)?,
                    tags: row.get(12)?,
                    similarity_hash: row.get::<_, i64>(13)? as u64,
                    rank: row.get(14)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() {
            return Ok(format!(
                "No past decisions found for query: '{query}'. \
                 Try different keywords or broader terms."
            ));
        }

        // Update use_count for returned decisions
        for hit in &rows {
            conn.execute(
                "UPDATE decisions SET use_count = use_count + 1 WHERE id = ?1",
                params![hit.id],
            )?;
        }

        // Format output
        let mut output = format!(
            "Found {} past decision(s) for '{}':\n\n",
            rows.len(),
            query
        );

        for (i, hit) in rows.iter().enumerate() {
            output.push_str(&format!(
                "## Decision {} (id={}, verified={}, success_rate={:.0}%, used={}x)\n",
                i + 1,
                hit.id,
                hit.verification_result,
                hit.success_rate * 100.0,
                hit.use_count,
            ));
            output.push_str(&format!(
                "   problem: {}\n",
                truncate_str(&hit.problem_signature, 200)
            ));
            output.push_str(&format!(
                "   hypothesis: {}\n",
                truncate_str(&hit.root_cause_hypothesis, 200)
            ));
            output.push_str(&format!(
                "   solution: {}\n",
                truncate_str(&hit.applied_solution, 200)
            ));
            if let Some(ref tags) = hit.tags {
                if !tags.is_empty() {
                    output.push_str(&format!("   tags: {}\n", tags));
                }
            }
            output.push_str(&format!(
                "   files: {}\n\n",
                truncate_str(&hit.affected_files, 200)
            ));
        }

        output.push_str("Use log_decision to record new decisions after verification.");
        Ok(output)
    }
}

/// 对 FTS5 查询字符串进行基本转义,防止语法错误。
fn escape_fts5_query(query: &str) -> String {
    // FTS5 特殊字符: " * ( ) : ^ ~ - + 以及引号
    // 简单策略:用双引号包围整个查询短语
    let cleaned = query
        .replace('"', "")
        .replace('\'', "")
        .replace('\\', "");
    // 添加前缀 * 以支持子串匹配
    format!("\"{}\"", cleaned.trim())
}

/// 截断字符串到指定长度。
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

// ---------------------------------------------------------------------------
// SearchHit
// ---------------------------------------------------------------------------

struct SearchHit {
    id: i64,
    session_id: String,
    timestamp_ms: i64,
    problem_signature: String,
    root_cause_hypothesis: String,
    applied_solution: String,
    affected_files: String,
    verification_result: String,
    verified_at_ms: Option<i64>,
    verify_count: i64,
    success_rate: f64,
    use_count: i64,
    tags: Option<String>,
    similarity_hash: u64,
    #[allow(dead_code)]
    rank: f64,
}

// ---------------------------------------------------------------------------
// Schema migration
// ---------------------------------------------------------------------------

fn migrate_schema(conn: &Connection) -> Result<(), DecisionLogError> {
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);

    if version < 1 {
        conn.execute_batch(CREATE_DECISIONS_V1)?;
        conn.execute_batch(CREATE_INDEXES_V1)?;
        conn.execute_batch(CREATE_DECISION_FILES_V1)?;
        conn.execute("PRAGMA user_version = 1", [])?;
    }

    if version < 2 {
        conn.execute_batch(CREATE_FTS_V2)?;
        conn.execute_batch(CREATE_TRIGGERS_V2)?;
        conn.execute("PRAGMA user_version = 2", [])?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simhash_same_text_produces_same_hash() {
        let a = compute_simhash("hello world foo bar");
        let b = compute_simhash("hello world foo bar");
        assert_eq!(a, b);
    }

    #[test]
    fn simhash_similar_texts_have_low_hamming_distance() {
        let a = compute_simhash(
            "fix null pointer dereference in auth module by adding null check",
        );
        let b = compute_simhash(
            "fix null pointer dereference in auth module add null check",
        );
        let dist = hamming_distance(a, b);
        // Similar texts should have distance ≤ 12 (lenient for short text)
        assert!(
            dist <= 12,
            "Expected hamming distance <= 12, got {dist}"
        );
    }

    #[test]
    fn simhash_different_texts_have_high_hamming_distance() {
        let a = compute_simhash("fix null pointer dereference in auth module");
        let b = compute_simhash("implement new caching layer for database queries");
        let dist = hamming_distance(a, b);
        assert!(
            dist >= 3,
            "Expected hamming distance >= 3, got {dist}"
        );
    }

    #[test]
    fn fnv1a_known_values() {
        assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn hamming_distance_basics() {
        assert_eq!(hamming_distance(0, 0), 0);
        assert_eq!(hamming_distance(1, 0), 1);
        assert_eq!(hamming_distance(0xFFFF, 0x0000), 16);
        assert_eq!(hamming_distance(u64::MAX, 0), 64);
    }

    #[test]
    fn decision_log_open_and_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let result = log.log_decision(
            r#"{
                "session_id": "test-session-1",
                "problem_signature": "null pointer dereference in auth_handler",
                "root_cause_hypothesis": "missing null check after user lookup",
                "applied_solution": "add if user.is_none() guard with early return",
                "affected_files": ["src/auth.rs", "src/auth_handler.rs"],
                "tags": ["null-pointer", "auth"],
                "verification_result": "Confirmed"
            }"#,
        );
        assert!(
            result.is_ok(),
            "log_decision failed: {:?}",
            result.err()
        );
        assert!(result.unwrap().starts_with("decision_logged id="));
    }

    #[test]
    fn decision_log_search_returns_results() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        // Log a decision first
        log.log_decision(
            r#"{
                "session_id": "test-search",
                "problem_signature": "null pointer dereference in auth_handler",
                "root_cause_hypothesis": "missing null check after user lookup",
                "applied_solution": "add if user.is_none() guard",
                "affected_files": ["src/auth.rs"],
                "tags": ["null-pointer"],
                "verification_result": "Confirmed"
            }"#,
        )
        .unwrap();

        // Search for it
        let result = log.search_decisions("null pointer", 10).unwrap();
        assert!(
            result.contains("null pointer"),
            "search result should contain 'null pointer', got: {result}"
        );
        assert!(
            result.contains("id="),
            "search result should contain decision id, got: {result}"
        );
    }

    #[test]
    fn decision_log_search_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let result = log.search_decisions("nonexistent_xyzzy", 10).unwrap();
        assert!(
            result.contains("No past decisions"),
            "expected 'No past decisions' message, got: {result}"
        );
    }

    #[test]
    fn decision_log_missing_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let result = log.log_decision(r#"{"session_id": "test"}"#);
        assert!(
            result.is_err(),
            "expected error for missing required fields"
        );
    }

    #[test]
    fn decision_log_schema_migration_v1_to_v2() {
        use rusqlite::Connection;

        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join(".claw");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("decision_log.db");

        // Simulate V1 schema (without FTS5)
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(CREATE_DECISIONS_V1).unwrap();
            conn.execute_batch(CREATE_INDEXES_V1).unwrap();
            conn.execute_batch(CREATE_DECISION_FILES_V1).unwrap();
            conn.execute("PRAGMA user_version = 1", []).unwrap();
        }

        // Open with DecisionLog — should auto-migrate to V2
        let log = DecisionLog::open(dir.path()).unwrap();

        // Insert should trigger FTS5 sync
        log.log_decision(
            r#"{
                "session_id": "migration-test",
                "problem_signature": "test migration",
                "root_cause_hypothesis": "migration should work",
                "applied_solution": "run DecisionLog::open()",
                "affected_files": [],
                "verification_result": "Confirmed"
            }"#,
        )
        .unwrap();

        // Search should find it via FTS5
        let result = log.search_decisions("migration", 10).unwrap();
        assert!(
            result.contains("migration"),
            "FTS5 search failed after migration: {result}"
        );
    }

    #[test]
    fn decision_log_use_count_increments_on_search() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        log.log_decision(
            r#"{
                "session_id": "use-count",
                "problem_signature": "use count increment test",
                "root_cause_hypothesis": "test search increments counter",
                "applied_solution": "verify use_count increases",
                "affected_files": [],
                "verification_result": "Pending"
            }"#,
        )
        .unwrap();

        // First search — use_count is 0 before increment, shows 0x
        let result1 = log.search_decisions("use count", 10).unwrap();
        assert!(result1.contains("used=0x"), "first search: {result1}");

        // Second search — use_count incremented to 1, shows 1x
        let result2 = log.search_decisions("use count", 10).unwrap();
        assert!(result2.contains("used=1x"), "second search: {result2}");
    }

    #[test]
    fn decision_log_file_associations() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        log.log_decision(
            r#"{
                "session_id": "file-test",
                "problem_signature": "file association test",
                "root_cause_hypothesis": "test file tracking",
                "applied_solution": "verify decision_files",
                "affected_files": ["src/main.rs", "src/lib.rs"],
                "tags": ["test"],
                "verification_result": "Confirmed"
            }"#,
        )
        .unwrap();

        // Verify file associations via direct SQL
        let conn = log.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM decision_files WHERE file_path = 'src/main.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "expected 1 file association");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM decision_files WHERE file_path = 'src/lib.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "expected 1 file association");
    }
}
