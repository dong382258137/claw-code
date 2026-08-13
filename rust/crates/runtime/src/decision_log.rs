//! DecisionLog — SQLite + FTS5-backed decision/repair experience index +
//! §4.7 设计决策自动提取(DecisionPoint)。
//!
//! # Design
//!
//! 本模块包含两类"决策"概念,正交共存:
//!
//! 1. **`DecisionLog`(§4.4 修复经验库)**:MemoryBank 核心组件,记录 LLM 每一次
//!    修复决策(问题签名、根因假设、应用方案、验证结果),提供语义检索与 simhash
//!    去重。由 LLM 主动调用 `log_decision` 工具记录。
//! 2. **`DecisionPoint`(§4.7 设计决策)**:在 context compaction 触发前自动
//!    启发式提取"设计决策点"(为什么选 A 不选 B、权衡了什么),持久化到
//!    NOTEBOOK.md `<decisions>` 段 + FTS5 history_index(role="decision")。
//!    与 `diag!` 互补:diag! 记录"发生了什么",DecisionPoint 记录"决定了什么"。
//!
//! ## 与 NOTEBOOK.md 分工
//!
//! - **DecisionLog** = 修复经验库(SQLite + simhash),持久化、可搜索、可去重
//! - **DecisionPoint** = 设计决策点(启发式提取 + NOTEBOOK 持久化)
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
use serde::{Deserialize, Serialize};

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
    for (i, item) in weights.iter_mut().enumerate() {
        if (hash >> i) & 1 == 1 {
            *item += 1;
        } else {
            *item -= 1;
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
// DecisionVerification
// ---------------------------------------------------------------------------

/// 决策验证结果(用于 `verify_decision` 学习环)。
///
/// 对应计划 §4.4 的 success_rate 学习公式:
/// - `Confirmed` — 决策成功复现,success_rate 增加趋近 1.0
/// - `Refuted`   — 决策被证伪,success_rate 衰减趋近 0.0
/// - `Partial`   — 部分有效,success_rate 介于两者之间
/// - `Pending`   — 重置为未验证状态,不更新统计(用于撤销之前的错误验证)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionVerification {
    Confirmed,
    Refuted,
    Partial,
    Pending,
}

impl DecisionVerification {
    /// 从字符串解析(接受大小写不敏感的 "confirmed"/"refuted"/"partial"/"pending")。
    pub fn from_str_ic(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "confirmed" => Some(Self::Confirmed),
            "refuted" => Some(Self::Refuted),
            "partial" => Some(Self::Partial),
            "pending" => Some(Self::Pending),
            _ => None,
        }
    }

    /// 序列化为 schema 中存储的字符串值(首字母大写形式)。
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Confirmed => "Confirmed",
            Self::Refuted => "Refuted",
            Self::Partial => "Partial",
            Self::Pending => "Pending",
        }
    }

    /// 学习增量:每次验证对 success_rate 的"信号值"贡献。
    ///
    /// 对应公式 `(success_rate * verify_count + signal) / (verify_count + 1)`:
    /// - Confirmed: signal = 1.0(完全成功)
    /// - Partial:   signal = 0.5(部分成功)
    /// - Refuted:   signal = 0.0(完全失败)
    /// - Pending:   不参与统计更新,signal 不被使用
    pub fn signal(&self) -> f64 {
        match self {
            Self::Confirmed => 1.0,
            Self::Partial => 0.5,
            Self::Refuted => 0.0,
            Self::Pending => 0.0,
        }
    }

    /// 是否更新统计字段(verify_count / success_rate)。
    /// Pending 用于"撤销"语义,不增加验证计数。
    pub fn updates_stats(&self) -> bool {
        !matches!(self, Self::Pending)
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
    /// 知识来源标签(Phase 1 知识新鲜度门控)。
    ///
    /// 值:"parametric"(参数记忆)/ "web_research"(联网调研)/ "unknown"(未门控)。
    /// 由 `NodeResult.gated.knowledge_source()` 映射,反映该决策基于何种知识。
    /// 缺失时(旧 JSON / 未门控路径)默认 "unknown"。
    pub knowledge_source: String,
}

impl DecisionRecord {
    fn from_json(input: &str) -> Result<Self, DecisionLogError> {
        let parsed: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| DecisionLogError::InvalidInput(format!("invalid JSON: {e}")))?;

        let session_id = parsed
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DecisionLogError::InvalidInput("missing 'session_id'".into()))?
            .to_string();

        let problem_signature = parsed
            .get("problem_signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DecisionLogError::InvalidInput("missing 'problem_signature'".into()))?
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
            .ok_or_else(|| DecisionLogError::InvalidInput("missing 'applied_solution'".into()))?
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

        let knowledge_source = parsed
            .get("knowledge_source")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(Self {
            session_id,
            problem_signature,
            root_cause_hypothesis,
            applied_solution,
            affected_files,
            tags,
            verification_result,
            verification_evidence,
            knowledge_source,
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

        let affected_files_json =
            serde_json::to_string(&record.affected_files).unwrap_or_else(|_| "[]".to_string());

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
                verification_evidence, context_hash, similarity_hash, tags,
                knowledge_source
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                record.knowledge_source,
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
    pub fn search_decisions(&self, query: &str, top_k: usize) -> Result<String, DecisionLogError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());

        // FTS5 search with BM25 ranking
        let mut stmt = conn.prepare(
            "SELECT d.id, d.session_id, d.timestamp_ms, d.problem_signature,
                    d.root_cause_hypothesis, d.applied_solution,
                    d.affected_files, d.verification_result, d.verified_at_ms,
                    d.verify_count, d.success_rate, d.use_count,
                    d.tags, d.similarity_hash, d.knowledge_source,
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
                    knowledge_source: row.get(14)?,
                    rank: row.get(15)?,
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
        let mut output = format!("Found {} past decision(s) for '{}':\n\n", rows.len(), query);

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
            // Phase 2.2:显示知识来源,让 LLM 知道该决策基于参数记忆还是联网调研
            output.push_str(&format!("   source: {}\n", hit.knowledge_source));
            output.push_str(&format!(
                "   files: {}\n\n",
                truncate_str(&hit.affected_files, 200)
            ));
        }

        output.push_str("Use log_decision to record new decisions after verification.");
        Ok(output)
    }

    /// Phase 2.2:按知识来源(knowledge_source)分组统计决策成功率。
    ///
    /// 用于闭环校准:对比 "web_research"(查来的方子)与 "parametric"(背出来的方子)
    /// 的平均 success_rate,反向评估知识新鲜度门控的有效性。
    /// 若 web_research 的成功率显著高于 parametric,说明门控有效;
    /// 若无差异或更低,说明调研质量不佳,需调整 build_research_query 或 assessor 阈值。
    ///
    /// # 输出格式
    /// ```text
    /// Knowledge source statistics:
    ///   parametric:      12 decisions, avg success_rate=62.5%
    ///   web_research:     5 decisions, avg success_rate=85.0%
    ///   deferred_research: 2 decisions, avg success_rate=40.0%
    ///   unknown:          1 decisions, avg success_rate=50.0%
    /// ```
    pub fn stats_by_knowledge_source(&self) -> Result<String, DecisionLogError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());

        let mut stmt = conn.prepare(
            "SELECT knowledge_source,
                    COUNT(*) as count,
                    AVG(success_rate) as avg_rate
             FROM decisions
             GROUP BY knowledge_source
             ORDER BY count DESC",
        )?;

        let rows: Vec<(String, i64, f64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() {
            return Ok("No decisions logged yet.".to_string());
        }

        let mut output = String::from("Knowledge source statistics:\n");
        for (source, count, avg_rate) in &rows {
            output.push_str(&format!(
                "  {source:<20} {count} decisions, avg success_rate={avg_rate:.1}%\n"
            ));
        }
        Ok(output)
    }

    /// 验证已有决策,更新 success_rate 学习环(计划 §4.4)。
    ///
    /// 接受 decision_id 和验证结果(Confirmed/Refuted/Partial/Pending),
    /// 原子地更新 verify_count、success_rate、verified_at_ms、
    /// verification_result、verification_evidence 字段。
    ///
    /// # Success Rate 公式
    ///
    /// 对 Confirmed/Partial/Refuted 三种"实质性验证":
    /// ```text
    /// new_success_rate = (old_success_rate * old_verify_count + signal)
    ///                  / (old_verify_count + 1);
    /// new_verify_count = old_verify_count + 1;
    /// ```
    /// 其中 `signal` 取值:Confirmed=1.0, Partial=0.5, Refuted=0.0。
    /// 这等价于把"是否成功"作为一个 Bernoulli 观测,以 running mean 形式
    /// 维护经验成功率,具有数学上的无偏性(多次 Confirmed 后趋近 1.0,
    /// 多次 Refuted 后趋近 0.0)。
    ///
    /// 对 Pending:只重置 verification_result/verified_at_ms,
    /// **不**更新 verify_count/success_rate(用于撤销之前的误验证)。
    ///
    /// # 事务原子性
    ///
    /// 整个更新过程包在 `BEGIN IMMEDIATE` 事务中,
    /// 防止并发 search_decisions 读到中间状态(verify_count 已更新但
    /// success_rate 未更新)。FTS5 同步触发器 `decisions_au` 在 UPDATE
    /// 时自动同步 FTS 索引,无需手动维护。
    ///
    /// # 参数
    ///
    /// - `decision_id` — 目标决策的 SQLite rowid(由 log_decision 返回)。
    /// - `result` — 验证结果枚举。
    /// - `evidence` — 可选的证据文本(测试输出、用户反馈等)。
    ///
    /// # 返回
    ///
    /// 成功时返回描述性字符串(含更新后的统计值),便于 LLM 上下文呈现。
    /// 决策不存在时返回 `DecisionLogError::InvalidInput`。
    pub fn verify_decision(
        &self,
        decision_id: i64,
        result: DecisionVerification,
        evidence: Option<&str>,
    ) -> Result<String, DecisionLogError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());

        // BEGIN IMMEDIATE 立刻获取写锁,避免 BEGIN DEFERRED 升级时的死锁。
        // SQLite 在 IMMEDIATE 事务中,其他 reader 会被阻塞直到 COMMIT,
        // 保证我们读到的 (verify_count, success_rate) 与最终写入一致。
        conn.execute_batch("BEGIN IMMEDIATE")?;

        // 用 transaction scope 保证任何错误路径都能自动 ROLLBACK。
        let outcome: Result<(), DecisionLogError> = (|| {
            // 先读取当前统计值(SELECT ... FOR UPDATE 语义,IMMEDIATE 锁已保证)。
            let row = conn
                .query_row(
                    "SELECT verify_count, success_rate FROM decisions WHERE id = ?1",
                    params![decision_id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
                )
                .map_err(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => DecisionLogError::InvalidInput(
                        format!("decision id={decision_id} not found"),
                    ),
                    other => DecisionLogError::Sqlite(other),
                })?;

            let (old_count, old_rate) = row;

            let (new_count, new_rate) = if result.updates_stats() {
                let signal = result.signal();
                let new_count = old_count + 1;
                // 注意 old_count 是 i64,需转 f64 防止整型除法丢失精度。
                let new_rate = (old_rate * old_count as f64 + signal) / (new_count as f64);
                // 钳位 [0.0, 1.0],防止浮点误差导致轻微越界。
                let new_rate = new_rate.clamp(0.0, 1.0);
                (new_count, new_rate)
            } else {
                // Pending: 不更新统计字段
                (old_count, old_rate)
            };

            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            // UPDATE 语句:即使 result == Pending,也要更新 verification_result/
            // verified_at_ms/verification_evidence 三个字段(语义:撤销原验证状态)。
            // 注意:此处 UPDATE 会触发 FTS5 触发器 decisions_au,
            // 但 FTS5 表只索引 problem_signature/root_cause_hypothesis/applied_solution
            // 三个字段,这些字段我们没改,触发器会以 new.* 形式重新插入索引行,
            // 行为是幂等的(no-op effect on FTS content)。
            let updated = conn.execute(
                "UPDATE decisions SET
                    verify_count = ?1,
                    success_rate = ?2,
                    verified_at_ms = ?3,
                    verification_result = ?4,
                    verification_evidence = ?5
                 WHERE id = ?6",
                params![
                    new_count,
                    new_rate,
                    now_ms,
                    result.as_db_str(),
                    evidence,
                    decision_id,
                ],
            )?;

            if updated == 0 {
                // 极端情况:查询时存在,UPDATE 时已被并发 DELETE。
                // 事务会自动 ROLLBACK,这里返回错误。
                return Err(DecisionLogError::InvalidInput(format!(
                    "decision id={decision_id} vanished during verify_decision"
                )));
            }

            Ok(())
        })();

        match outcome {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                // COMMIT 后再读取一次,得到最终状态用于返回消息。
                // 也可以直接用 new_count/new_rate,但重新读取能验证事务真的提交了。
                let (final_count, final_rate, final_result) = conn
                    .query_row(
                        "SELECT verify_count, success_rate, verification_result
                         FROM decisions WHERE id = ?1",
                        params![decision_id],
                        |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                r.get::<_, f64>(1)?,
                                r.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .map_err(DecisionLogError::Sqlite)?;

                Ok(format!(
                    "decision_verified id={decision_id} result={final_result} \
                     verify_count={final_count} success_rate={:.6} \
                     evidence_provided={}",
                    final_rate,
                    if evidence.is_some() { "yes" } else { "no" },
                ))
            }
            Err(e) => {
                // 任何错误都先尝试 ROLLBACK;若 ROLLBACK 失败则用原始错误报告,
                // 因为连接已经处于不一致状态,后续操作都会失败。
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

/// 对 FTS5 查询字符串进行基本转义,防止语法错误。
fn escape_fts5_query(query: &str) -> String {
    // FTS5 特殊字符: " * ( ) : ^ ~ - + 以及引号
    // 简单策略:用双引号包围整个查询短语
    let cleaned = query.replace(['\"', '\'', '\\'], "");
    // 添加前缀 * 以支持子串匹配
    format!("\"{}\"", cleaned.trim())
}
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        // UTF-8 安全截断：用 floor_char_boundary 找到不超过 max_len 的最大字符边界。
        // 如果 max_len 落在多字节字符中间，回退到上一个字符边界。
        // std::str::floor_char_boundary 在 Rust 1.82+ 稳定；
        // 为了兼容旧版本，手动实现等价逻辑。
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            // max_len 太小，连一个字符都放不下，返回省略号
            "...".to_string()
        } else {
            format!("{}...", &s[..end])
        }
    }
}

// ---------------------------------------------------------------------------
// SearchHit
// ---------------------------------------------------------------------------

#[allow(dead_code)]
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
    /// Phase 2.2:知识来源(parametric/web_research/deferred_research/unknown)。
    knowledge_source: String,
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

    if version < 3 {
        // Phase 1 知识新鲜度门控:加 knowledge_source 列。
        // ALTER TABLE ADD COLUMN 带默认值,旧数据自动填 "unknown"。
        // 新列不参与 FTS5 索引(非搜索字段),无需重建触发器。
        conn.execute_batch(
            "ALTER TABLE decisions ADD COLUMN knowledge_source TEXT NOT NULL DEFAULT 'unknown';",
        )?;
        conn.execute("PRAGMA user_version = 3", [])?;
    }

    Ok(())
}

// ===========================================================================
// §4.7 设计决策自动提取(DecisionPoint)— compaction 前持久化设计决策
//
// 与 §4.4 的 `DecisionLog`(修复经验库)正交:
// - `DecisionLog` = LLM 主动调用的 SQLite 修复经验库
// - `DecisionPoint` = compaction 前自动启发式提取的设计决策,持久化到 NOTEBOOK.md
//
// 与 §4.1 的 `diag!` 互补:
// - `diag!` 记录"发生了什么"(运行时信号:panic/error/状态变更)
// - `DecisionPoint` 记录"决定了什么、为什么"(设计决策信号)
// ===========================================================================

/// 决策点 — 一个关键设计决策的完整记录(§4.7)。
///
/// 在 context compaction 触发前,从待压缩消息中启发式提取,
/// 持久化到 NOTEBOOK.md `<decisions>` 段 + FTS5 history_index(role="decision")。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionPoint {
    /// 决策 ID(时间戳 + 消息序号哈希)。
    pub id: String,
    /// 决策上下文(什么场景下做的决策,通常是前一条消息)。
    pub context: String,
    /// 决策内容(做了什么决定)。
    pub decision: String,
    /// 决策理由(为什么这样做)。
    pub rationale: String,
    /// 被否决的替代方案(为什么没选其他选项)。
    /// MVP 启发式不提取,留空;v2 LLM 提取时填充。
    pub alternatives: Vec<String>,
    /// 时间戳(ms since UNIX_EPOCH)。
    pub timestamp_ms: u64,
    /// 来源 session_id。
    pub session_id: String,
}

/// 决策检测策略(§4.7)。
#[derive(Debug, Clone, PartialEq)]
pub enum DetectionStrategy {
    /// MVP:启发式关键词检测。零 LLM 调用,零成本。
    /// 检测包含决策信号的消息:decided/chose/trade-off/权衡/否决/放弃/之所以/因为。
    Heuristic,
    /// v2:LLM 提取。用轻量模型(flash)从待压缩消息中提取结构化决策。
    /// 成本低(flash),但需要 LLM 调用。
    ///
    /// **使用方式**:`extract_decisions_before_compaction` 中 LlmExtract 分支
    /// 需要通过 [`set_global_decision_extractor_client`] 注册全局 client 才能工作。
    /// 未注册时降级为 Heuristic(零成本回退,保证不阻塞 compaction)。
    LlmExtract { model: String },
}

/// 决策提取 client trait — v2 §10.5 Epic 6 依赖倒置。
///
/// runtime crate 不直接依赖 api crate(避免循环依赖),通过此 trait 注入 LLM 调用。
/// 生产实现由上层 crate 构造(封装 `ProviderClient::from_model` + async-to-sync 桥接)。
///
/// # 接口约定
/// - 输入:提取 prompt(含待压缩消息列表 + JSON schema 说明)
/// - 输出:LLM 原始文本响应(应为 JSON 数组,由 `parse_llm_decision_json` 解析)
/// - 错误:网络/API/超时等返回 `Err(String)`,调用方降级为 Heuristic
pub trait DecisionExtractorClient: Send + Sync {
    /// 调用 LLM 提取决策,返回原始文本响应。
    fn extract(&self, prompt: &str) -> Result<String, String>;
}

/// 全局决策提取 client(OnceLock,进程级单例)。
///
/// v2 设计:通过 `set_global_decision_extractor_client` 在进程启动时注入,
/// `extract_decisions_before_compaction` 的 LlmExtract 分支通过
/// `global_decision_extractor_client()` 获取并调用。
///
/// 未注入时 LlmExtract 降级为 Heuristic(零成本回退)。
static GLOBAL_DECISION_EXTRACTOR: std::sync::OnceLock<
    Option<std::sync::Arc<dyn DecisionExtractorClient>>,
> = std::sync::OnceLock::new();

/// 注册全局决策提取 client(v2 §10.5 Epic 6)。
///
/// 由上层 crate(rusty-claude-cli)在启动时调用,注入 `Arc<dyn DecisionExtractorClient>`。
/// 注入后 `DetectionStrategy::LlmExtract` 才能真正调用 LLM。
pub fn set_global_decision_extractor_client(client: std::sync::Arc<dyn DecisionExtractorClient>) {
    let _ = GLOBAL_DECISION_EXTRACTOR.set(Some(client));
}

/// v3:检查全局 DecisionExtractorClient 是否已注册。
///
/// 供 `/detection-strategy --verify` 命令在切换前预检 client 可用性。
/// 返回 `false` 时,`LlmExtract` 策略会自动降级为 `Heuristic`。
#[must_use]
pub fn is_decision_extractor_client_registered() -> bool {
    GLOBAL_DECISION_EXTRACTOR
        .get()
        .is_some_and(|opt| opt.is_some())
}

/// 获取全局决策提取 client(若已注册)。
fn global_decision_extractor_client() -> Option<&'static std::sync::Arc<dyn DecisionExtractorClient>>
{
    GLOBAL_DECISION_EXTRACTOR.get().and_then(|opt| opt.as_ref())
}

/// 启发式决策检测关键词(中英文,§4.7)。
/// MVP 策略:零成本,零 LLM 调用。
///
/// 注意:避免加入过于宽泛的词(如 "over "),否则会误匹配普通文本。
const DECISION_KEYWORDS: &[&str] = &[
    // 英文决策信号
    "decided",
    "chose",
    "chosen",
    "trade-off",
    "tradeoff",
    "alternative",
    "rejected",
    "ruled out",
    "instead of",
    "rather than",
    // 中文决策信号
    "决定",
    "选择",
    "权衡",
    "否决",
    "放弃",
    "之所以",
    "因为",
    "而非",
    "而不是",
    "替代方案",
    "备选",
];

/// 检测单条消息是否包含决策信号(§4.7)。
///
/// MVP:关键词匹配(大小写不敏感)。v2:LLM 提取。
pub fn detect_decision_signal(message_text: &str) -> bool {
    let lower = message_text.to_ascii_lowercase();
    DECISION_KEYWORDS
        .iter()
        .any(|kw| lower.contains(&kw.to_ascii_lowercase()))
}

/// 从待压缩的消息列表中提取决策点(§4.7)。
///
/// 在 `compact_session` 执行前调用,确保设计决策不随原始消息消失。
///
/// # MVP 策略(`DetectionStrategy::Heuristic`)
///
/// 1. 遍历待压缩消息,检测决策关键词
/// 2. 命中关键词的消息,提取该消息 + 前一条消息作为上下文
/// 3. 用模板构造 `DecisionPoint`(context/decision/rationale 由 LLM 后续填充或人工确认)
///
/// # v2 策略(`DetectionStrategy::LlmExtract`)
///
/// 1. 用 flash 模型从待压缩消息中提取结构化决策
/// 2. 自动填充 context/decision/rationale/alternatives
/// 3. MVP 阶段直接返回空 Vec(`TODO(v2)` 标记)
///
/// # 参数
///
/// - `messages`:待压缩的消息文本列表(已转换为纯文本)
/// - `strategy`:检测策略(MVP 用 `Heuristic`,v2 用 `LlmExtract`)
/// - `session_id`:当前 session ID,用于追溯决策来源
///
/// # 返回
///
/// 提取到的决策点列表(可能为空)。每个决策点 ID 唯一,格式 `d{timestamp_ms}-{index}`。
pub fn extract_decisions_before_compaction(
    messages: &[&str],
    strategy: &DetectionStrategy,
    session_id: &str,
) -> Vec<DecisionPoint> {
    match strategy {
        DetectionStrategy::Heuristic => {
            let mut decisions = Vec::new();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            for (i, msg) in messages.iter().enumerate() {
                if detect_decision_signal(msg) {
                    // 提取上下文:前一条消息(如果有)
                    let context = if i > 0 { messages[i - 1] } else { "" };
                    // MVP:整条消息作为 decision + rationale 的候选
                    // v2 会用 LLM 精确提取
                    let id = format!("d{now_ms}-{i}");
                    decisions.push(DecisionPoint {
                        id,
                        context: truncate_for_decision(context, 200),
                        decision: truncate_for_decision(msg, 300),
                        rationale: truncate_for_decision(msg, 500),
                        alternatives: Vec::new(), // MVP 不提取,留待 v2 LLM 填充
                        timestamp_ms: now_ms,
                        session_id: session_id.to_string(),
                    });
                }
            }
            decisions
        }
        DetectionStrategy::LlmExtract { model } => {
            // Tier S #3 穷鬼模式:激活时跳过 LLM 决策提取,回退纯启发式(省 token)。
            if crate::poor_mode::is_active() {
                let heuristic_strategy = DetectionStrategy::Heuristic;
                return extract_decisions_before_compaction(
                    messages,
                    &heuristic_strategy,
                    session_id,
                );
            }
            // v2 §10.5 Epic 6 实现:调用全局 DecisionExtractorClient 提取决策
            //
            // 降级策略:若未注册全局 client,回退到 Heuristic(零成本,不阻塞 compaction)
            match global_decision_extractor_client() {
                Some(client) => {
                    extract_decisions_with_llm(messages, model, client.as_ref(), session_id)
                }
                None => {
                    eprintln!(
                        "[decision_log] LlmExtract strategy requested but no global client registered — \
                         falling back to Heuristic"
                    );
                    // 降级:用 Heuristic 逻辑提取(避免完全丢失决策)
                    let heuristic_strategy = DetectionStrategy::Heuristic;
                    extract_decisions_before_compaction(messages, &heuristic_strategy, session_id)
                }
            }
        }
    }
}

/// UTF-8 安全的字符串截断(§4.7 内部辅助)。
///
/// 与现有 `truncate_str` 类似,但使用省略号 `…`(单字符)而非 `...`(三字符),
/// 与计划文档保持一致。若 `max` 落在多字节字符中间,回退到上一个字符边界。
fn truncate_for_decision(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return "…".to_string();
    }
    format!("{}…", &s[..end])
}

/// v2 §10.5 Epic 6:用 LLM 从待压缩消息中提取结构化决策点。
///
/// # 流程
/// 1. 构造提取 prompt(消息列表 + JSON schema 说明 + few-shot 示例)
/// 2. 调用 `DecisionExtractorClient::extract`
/// 3. 解析 JSON 数组(容错:剥离 markdown 代码块、字段缺失跳过)
/// 4. 截断字段(context 200 / decision 300 / rationale 500 / alternatives 100)
///
/// # 降级策略
/// - LLM 调用失败 → 回退到 Heuristic(保证不丢决策)
/// - JSON 解析失败 → 回退到 Heuristic
/// - 部分条目解析失败 → 跳过该条,保留成功解析的条目
///
/// # 参数
/// - `messages`:待压缩的消息文本列表
/// - `model`:LLM 模型名(仅用于诊断日志,实际调用由 client 封装)
/// - `client`:决策提取 client(依赖倒置)
/// - `session_id`:当前 session ID
fn extract_decisions_with_llm(
    messages: &[&str],
    model: &str,
    client: &dyn DecisionExtractorClient,
    session_id: &str,
) -> Vec<DecisionPoint> {
    // 1. 构造 prompt
    let prompt = build_llm_extract_prompt(messages);

    // 2. 调用 LLM
    let response = match client.extract(&prompt) {
        Ok(resp) => resp,
        Err(e) => {
            eprintln!(
                "[decision_log] LLM extract failed (model={model}): {e} — falling back to Heuristic"
            );
            let heuristic_strategy = DetectionStrategy::Heuristic;
            return extract_decisions_before_compaction(messages, &heuristic_strategy, session_id);
        }
    };

    // 3. 解析 JSON
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    match parse_llm_decision_json(&response, now_ms, session_id) {
        Ok(decisions) if !decisions.is_empty() => {
            eprintln!(
                "[decision_log] LLM extracted {} decision point(s) (model={model})",
                decisions.len()
            );
            decisions
        }
        Ok(_) => {
            // LLM 返回空数组,回退到 Heuristic(可能 LLM 漏掉了决策)
            eprintln!(
                "[decision_log] LLM returned 0 decisions (model={model}) — falling back to Heuristic"
            );
            let heuristic_strategy = DetectionStrategy::Heuristic;
            extract_decisions_before_compaction(messages, &heuristic_strategy, session_id)
        }
        Err(e) => {
            eprintln!(
                "[decision_log] LLM JSON parse failed (model={model}): {e} — falling back to Heuristic"
            );
            let heuristic_strategy = DetectionStrategy::Heuristic;
            extract_decisions_before_compaction(messages, &heuristic_strategy, session_id)
        }
    }
}

/// 构造 LLM 提取 prompt(v2 §10.5 Epic 6)。
fn build_llm_extract_prompt(messages: &[&str]) -> String {
    let messages_block = messages
        .iter()
        .enumerate()
        .map(|(i, msg)| format!("[{i}] {msg}"))
        .collect::<Vec<_>>()
        .join("\n---\n");

    let mut prompt = String::new();
    prompt.push_str("你是一个决策提取助手。请从以下对话消息中提取\"设计决策点\"。\n\n");
    prompt.push_str("## 设计决策点的判定标准\n");
    prompt.push_str("- 明确选择了某个方案(而非仅陈述事实)\n");
    prompt.push_str("- 包含 trade-off 权衡或否决其他方案的理由\n");
    prompt.push_str("- 涉及架构、技术选型、向后兼容、迁移策略等关键决策\n");
    prompt.push_str("- **不提取**:纯事实陈述、问题报告、代码片段、操作步骤\n\n");
    prompt.push_str("## 输出格式\n");
    prompt.push_str("输出 JSON 数组,每个元素包含:\n");
    prompt.push_str("```json\n");
    prompt.push_str("[\n");
    prompt.push_str("  {\n");
    prompt.push_str("    \"context\": \"决策前的上下文(≤200字符)\",\n");
    prompt.push_str("    \"decision\": \"做了什么决定(≤300字符)\",\n");
    prompt.push_str("    \"rationale\": \"为什么这样做(≤500字符)\",\n");
    prompt.push_str("    \"alternatives\": [\"被否决的方案1\", \"被否决的方案2\"]\n");
    prompt.push_str("  }\n");
    prompt.push_str("]\n");
    prompt.push_str("```\n\n");
    prompt.push_str("## few-shot 示例\n");
    prompt.push_str("输入: [0] 我们考虑了 Rust 和 Go,最终决定用 Rust,因为性能更好且内存安全\n");
    prompt.push_str("输出: [{\"context\": \"语言选型\", \"decision\": \"选择 Rust\", \"rationale\": \"性能更好且内存安全\", \"alternatives\": [\"Go\"]}]\n\n");
    prompt.push_str("若无决策点,返回空数组 `[]`。\n\n");
    prompt.push_str("## 待分析的消息\n");
    prompt.push_str(&messages_block);
    prompt
}

/// 解析 LLM 返回的决策 JSON(v2 §10.5 Epic 6)。
///
/// # 容错策略
/// 1. 剥离 markdown 代码块包裹(```json ... ``` 或 ``` ... ```)
/// 2. 提取首个 JSON 数组(从 `[` 到匹配的 `]`)
/// 3. 用 `serde_json::from_str` 解析为 `Vec<RawDecision>`
/// 4. 逐条转换,跳过字段缺失/类型错误的条目
/// 5. 截断字段到上限
fn parse_llm_decision_json(
    response: &str,
    now_ms: u64,
    session_id: &str,
) -> Result<Vec<DecisionPoint>, String> {
    // 1. 剥离 markdown 代码块
    let json_str = strip_markdown_code_block(response);

    // 2. 提取首个 JSON 数组
    let json_array = extract_json_array(json_str)?;

    // 3. 解析为 Vec<RawDecision>
    let raw_decisions: Vec<RawDecision> =
        serde_json::from_str(json_array).map_err(|e| format!("JSON 解析失败: {e}"))?;

    // 4. 逐条转换 + 截断
    let mut decisions = Vec::new();
    for (i, raw) in raw_decisions.into_iter().enumerate() {
        // 跳过 decision 为空的条目(无决策内容)
        if raw.decision.trim().is_empty() {
            continue;
        }
        let id = format!("d{now_ms}-llm-{i}");
        let alternatives: Vec<String> = raw
            .alternatives
            .into_iter()
            .map(|a| truncate_for_decision(&a, 100))
            .collect();
        decisions.push(DecisionPoint {
            id,
            context: truncate_for_decision(&raw.context, 200),
            decision: truncate_for_decision(&raw.decision, 300),
            rationale: truncate_for_decision(&raw.rationale, 500),
            alternatives,
            timestamp_ms: now_ms,
            session_id: session_id.to_string(),
        });
    }

    Ok(decisions)
}

/// 剥离 markdown 代码块包裹(```json ... ``` 或 ``` ... ```)。
#[allow(clippy::manual_strip)]
fn strip_markdown_code_block(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.starts_with("```") {
        let after_first_line = trimmed.strip_prefix("```").unwrap_or(trimmed);
        // 跳过语言标识(如 json)
        let after_lang = if after_first_line.starts_with("json") {
            &after_first_line[4..]
        } else {
            after_first_line
        };
        // 去掉末尾的 ```
        if let Some(stripped) = after_lang.strip_suffix("```") {
            return stripped.trim();
        }
        // 末尾无 ``` 也接受(容错)
        return after_lang.trim();
    }
    trimmed
}

/// 从文本中提取首个 JSON 数组(从 `[` 到匹配的 `]`)。
fn extract_json_array(s: &str) -> Result<&str, String> {
    let start = s
        .find('[')
        .ok_or_else(|| "无 JSON 数组起始 `[`".to_string())?;
    // 简化策略:从最后一个 `]` 截断(容忍中间嵌套)
    if let Some(end) = s.rfind(']') {
        if end > start {
            return Ok(&s[start..=end]);
        }
    }
    Err("无 JSON 数组结束 `]`".to_string())
}

/// LLM 返回的原始决策(用于 serde 反序列化)。
#[derive(Debug, serde::Deserialize)]
struct RawDecision {
    #[serde(default)]
    context: String,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    rationale: String,
    #[serde(default)]
    alternatives: Vec<String>,
}

/// 将决策点渲染为 NOTEBOOK.md `<decisions>` 段的格式(§4.7)。
///
/// 输出格式与现有 NOTEBOOK 段一致(纯文本列表,每行一条决策)。
/// 最多渲染 20 条决策(NOTEBOOK 16K 上限的自我约束)。
#[must_use]
pub fn render_decision_for_notebook(decisions: &[DecisionPoint]) -> String {
    if decisions.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for d in decisions.iter().take(20) {
        // ID 取前 8 字符(与计划一致),避免过长
        let id_short: String = d.id.chars().take(8).collect();
        lines.push(format!("- [{}] {} — {}", id_short, d.decision, d.rationale));
        if !d.alternatives.is_empty() {
            lines.push(format!("  alternatives: {}", d.alternatives.join("; ")));
        }
    }
    lines.join("\n")
}

/// 将决策点追加到 NOTEBOOK.md 的 `<decisions>` 段(§4.7)。
///
/// 复用 `Notebook::save` 的原子写机制(`.tmp` + `rename`),遵守 16K 上限。
/// 超限时保留最近 100 行(约 10-20 条决策),丢弃旧决策。
///
/// # 参数
///
/// - `workspace_root`:工作区根目录(NOTEBOOK.md 位于 `.claw/NOTEBOOK.md`)
/// - `decisions`:待持久化的决策点列表
///
/// # 返回
///
/// `Ok(())` 表示成功持久化;`Err(msg)` 表示加载/保存失败。
pub fn persist_decisions_to_notebook(
    workspace_root: &std::path::Path,
    decisions: &[DecisionPoint],
) -> Result<(), String> {
    if decisions.is_empty() {
        return Ok(());
    }
    let mut notebook = crate::notebook::Notebook::load(workspace_root)
        .map_err(|e| format!("load notebook failed: {e}"))?;
    let rendered = render_decision_for_notebook(decisions);
    if rendered.is_empty() {
        return Ok(());
    }
    // 追加到 decisions 段(不覆盖,累积记录)
    let existing = notebook.get_section("decisions").unwrap_or_default();
    let combined = if existing.is_empty() {
        rendered
    } else {
        format!("{existing}\n{rendered}")
    };
    // 检查上限:NOTEBOOK_MAX_CHARS = 16_000,decisions 段预留 14_000 字符
    // 超限时保留最近 100 行(从末尾截取),丢弃旧决策
    if combined.len() > 14_000 {
        let lines: Vec<&str> = combined.lines().collect();
        let start = lines.len().saturating_sub(100);
        let trimmed = lines[start..].join("\n");
        notebook.set_section("decisions", &trimmed);
    } else {
        notebook.set_section("decisions", &combined);
    }
    notebook
        .save(workspace_root)
        .map_err(|e| format!("save notebook failed: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_handles_multibyte_utf8_safely() {
        // ASCII：正常截断
        assert_eq!(truncate_str("hello world", 5), "hello...");
        // 不需要截断
        assert_eq!(truncate_str("short", 200), "short");
        // 中文：3 字节字符，max_len 落在字符中间不应 panic
        let chinese = "你好世界测试字符串";
        let result = truncate_str(chinese, 7); // 7 落在第二个中文字符（字节 3-5）的中间
        assert!(result.ends_with("..."));
        assert!(!result.is_empty());
        // 结果应该是 "你好..."（截到字节 6，即第二个完整中文字符之后）
        assert_eq!(result, "你好...");
        // Emoji：4 字节字符
        let emoji = "a🎉b🎊c";
        let result = truncate_str(emoji, 2); // 2 落在 emoji（字节 1-4）中间
        assert_eq!(result, "a...");
        // max_len = 0 的极端情况
        assert_eq!(truncate_str("hello", 0), "...");
    }

    #[test]
    fn simhash_same_text_produces_same_hash() {
        let a = compute_simhash("hello world foo bar");
        let b = compute_simhash("hello world foo bar");
        assert_eq!(a, b);
    }

    #[test]
    fn simhash_similar_texts_have_low_hamming_distance() {
        let a = compute_simhash("fix null pointer dereference in auth module by adding null check");
        let b = compute_simhash("fix null pointer dereference in auth module add null check");
        let dist = hamming_distance(a, b);
        // Similar texts should have distance ≤ 12 (lenient for short text)
        assert!(dist <= 12, "Expected hamming distance <= 12, got {dist}");
    }

    #[test]
    fn simhash_different_texts_have_high_hamming_distance() {
        let a = compute_simhash("fix null pointer dereference in auth module");
        let b = compute_simhash("implement new caching layer for database queries");
        let dist = hamming_distance(a, b);
        assert!(dist >= 3, "Expected hamming distance >= 3, got {dist}");
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
        assert!(result.is_ok(), "log_decision failed: {:?}", result.err());
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

    // -----------------------------------------------------------------
    // DecisionVerification 枚举测试
    // -----------------------------------------------------------------

    #[test]
    fn decision_verification_from_str_ic_handles_case_variants() {
        assert_eq!(
            DecisionVerification::from_str_ic("Confirmed"),
            Some(DecisionVerification::Confirmed)
        );
        assert_eq!(
            DecisionVerification::from_str_ic("REFUTED"),
            Some(DecisionVerification::Refuted)
        );
        assert_eq!(
            DecisionVerification::from_str_ic("partial"),
            Some(DecisionVerification::Partial)
        );
        assert_eq!(
            DecisionVerification::from_str_ic("  pending  "),
            Some(DecisionVerification::Pending)
        );
        assert_eq!(DecisionVerification::from_str_ic("bogus"), None);
        assert_eq!(DecisionVerification::from_str_ic(""), None);
    }

    #[test]
    fn decision_verification_as_db_str_round_trips() {
        for v in [
            DecisionVerification::Confirmed,
            DecisionVerification::Refuted,
            DecisionVerification::Partial,
            DecisionVerification::Pending,
        ] {
            let s = v.as_db_str();
            assert_eq!(DecisionVerification::from_str_ic(s), Some(v));
        }
    }

    #[test]
    fn decision_verification_signal_and_updates_stats_consistency() {
        // 实质性验证必须 updates_stats == true 且 signal 在 [0, 1]
        for v in [
            DecisionVerification::Confirmed,
            DecisionVerification::Partial,
            DecisionVerification::Refuted,
        ] {
            assert!(v.updates_stats(), "{v:?} should update stats");
            let sig = v.signal();
            assert!(
                (0.0..=1.0).contains(&sig),
                "{v:?} signal {sig} out of [0,1]"
            );
        }
        // Pending 不更新统计
        assert!(!DecisionVerification::Pending.updates_stats());
    }

    // -----------------------------------------------------------------
    // verify_decision 单元测试
    // -----------------------------------------------------------------

    /// 辅助函数:从 log_decision 的返回字符串 "decision_logged id=N" 中解析出 id。
    fn parse_decision_id(s: &str) -> i64 {
        // 形如 "decision_logged id=42"
        s.split("id=")
            .nth(1)
            .expect("missing id= in log_decision output")
            .trim()
            .parse::<i64>()
            .expect("id is not a valid i64")
    }

    /// 辅助函数:从 verify_decision 返回字符串中解析 success_rate。
    fn parse_success_rate(s: &str) -> f64 {
        // 形如 "decision_verified id=1 result=Confirmed verify_count=1 success_rate=1.0000 evidence_provided=no"
        let part = s
            .split("success_rate=")
            .nth(1)
            .expect("missing success_rate= in verify_decision output");
        let token = part.split_whitespace().next().expect("empty rate token");
        token.parse::<f64>().expect("rate is not a valid f64")
    }

    /// 辅助函数:从 verify_decision 返回字符串中解析 verify_count。
    fn parse_verify_count(s: &str) -> i64 {
        let part = s
            .split("verify_count=")
            .nth(1)
            .expect("missing verify_count= in verify_decision output");
        let token = part.split_whitespace().next().expect("empty count token");
        token.parse::<i64>().expect("count is not a valid i64")
    }

    #[test]
    fn verify_decision_confirmed_increments_rate_toward_one() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let id = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "verify-test",
                    "problem_signature": "test confirmed",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": [],
                    "verification_result": "Pending"
                }"#,
            )
            .unwrap(),
        );

        // 初始: verify_count=0, success_rate=0.0
        let out1 = log
            .verify_decision(id, DecisionVerification::Confirmed, Some("tests pass"))
            .unwrap();
        // 公式: (0.0 * 0 + 1.0) / (0 + 1) = 1.0
        assert_eq!(parse_verify_count(&out1), 1);
        assert!(
            (parse_success_rate(&out1) - 1.0).abs() < 1e-5,
            "Confirmed #1: rate should be 1.0, got {out1}"
        );

        // 再次 Confirmed: (1.0 * 1 + 1.0) / 2 = 1.0
        let out2 = log
            .verify_decision(id, DecisionVerification::Confirmed, None)
            .unwrap();
        assert_eq!(parse_verify_count(&out2), 2);
        assert!(
            (parse_success_rate(&out2) - 1.0).abs() < 1e-5,
            "Confirmed #2: rate should remain 1.0, got {out2}"
        );
    }

    #[test]
    fn verify_decision_refuted_decays_rate_toward_zero() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let id = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "verify-test",
                    "problem_signature": "test refuted",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": []
                }"#,
            )
            .unwrap(),
        );

        // Confirmed 一次: rate = 1.0
        log.verify_decision(id, DecisionVerification::Confirmed, None)
            .unwrap();

        // Refuted: (1.0 * 1 + 0.0) / 2 = 0.5
        let out = log
            .verify_decision(id, DecisionVerification::Refuted, Some("tests fail"))
            .unwrap();
        assert_eq!(parse_verify_count(&out), 2);
        assert!(
            (parse_success_rate(&out) - 0.5).abs() < 1e-5,
            "Refuted after Confirmed: rate should be 0.5, got {out}"
        );

        // 再 Refuted: (0.5 * 2 + 0.0) / 3 = 1/3 ≈ 0.3333
        let out2 = log
            .verify_decision(id, DecisionVerification::Refuted, None)
            .unwrap();
        assert_eq!(parse_verify_count(&out2), 3);
        assert!(
            (parse_success_rate(&out2) - 1.0 / 3.0).abs() < 1e-5,
            "Refuted #2: rate should be 1/3, got {out2}"
        );
    }

    #[test]
    fn verify_decision_partial_yields_half_signal() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let id = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "verify-test",
                    "problem_signature": "test partial",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": []
                }"#,
            )
            .unwrap(),
        );

        // Partial 单次: (0.0 * 0 + 0.5) / 1 = 0.5
        let out = log
            .verify_decision(id, DecisionVerification::Partial, Some("flaky"))
            .unwrap();
        assert_eq!(parse_verify_count(&out), 1);
        assert!(
            (parse_success_rate(&out) - 0.5).abs() < 1e-5,
            "Partial #1: rate should be 0.5, got {out}"
        );

        // 再次 Partial: (0.5 * 1 + 0.5) / 2 = 0.5
        let out2 = log
            .verify_decision(id, DecisionVerification::Partial, None)
            .unwrap();
        assert_eq!(parse_verify_count(&out2), 2);
        assert!(
            (parse_success_rate(&out2) - 0.5).abs() < 1e-5,
            "Partial #2: rate should remain 0.5, got {out2}"
        );
    }

    #[test]
    fn verify_decision_pending_does_not_touch_stats() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let id = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "verify-test",
                    "problem_signature": "test pending",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": [],
                    "verification_result": "Confirmed"
                }"#,
            )
            .unwrap(),
        );

        // 先 Confirmed 提升到 1.0
        log.verify_decision(id, DecisionVerification::Confirmed, None)
            .unwrap();
        // 现在 verify_count=1, success_rate=1.0

        // Pending: 不应改 verify_count/success_rate,但应改 verification_result
        let out = log
            .verify_decision(id, DecisionVerification::Pending, None)
            .unwrap();
        assert!(
            out.contains("result=Pending"),
            "Pending should reset verification_result, got {out}"
        );
        assert_eq!(
            parse_verify_count(&out),
            1,
            "Pending must NOT increment verify_count, got {out}"
        );
        assert!(
            (parse_success_rate(&out) - 1.0).abs() < 1e-5,
            "Pending must NOT change success_rate, got {out}"
        );

        // 直接 SQL 验证 verified_at_ms 被更新为非空
        let conn = log.conn.lock().unwrap();
        let verified_at: Option<i64> = conn
            .query_row(
                "SELECT verified_at_ms FROM decisions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert!(verified_at.is_some(), "verified_at_ms should be set");
    }

    #[test]
    fn verify_decision_nonexistent_id_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let err = log
            .verify_decision(99999, DecisionVerification::Confirmed, None)
            .unwrap_err();
        match err {
            DecisionLogError::InvalidInput(msg) => {
                assert!(
                    msg.contains("not found") || msg.contains("99999"),
                    "error should mention id 99999, got: {msg}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn verify_decision_evidence_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let id = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "verify-test",
                    "problem_signature": "evidence test",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": []
                }"#,
            )
            .unwrap(),
        );

        log.verify_decision(
            id,
            DecisionVerification::Confirmed,
            Some("cargo test passed: 42 passed, 0 failed"),
        )
        .unwrap();

        let conn = log.conn.lock().unwrap();
        let evidence: Option<String> = conn
            .query_row(
                "SELECT verification_evidence FROM decisions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(
            evidence.as_deref(),
            Some("cargo test passed: 42 passed, 0 failed"),
            "evidence should be persisted verbatim"
        );
    }

    #[test]
    fn verify_decision_search_still_works_after_update() {
        // 验证 FTS5 触发器 decisions_au 在 UPDATE 后仍保持索引一致。
        // 之前 UPDATE 会触发 trigger 重新同步 FTS,如果 trigger 在
        // 事务中失败,COMMIT 会回滚整个 verify_decision。
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let id = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "verify-test",
                    "problem_signature": "fts trigger test unique_marker_alpha",
                    "root_cause_hypothesis": "hypothesis_marker_beta",
                    "applied_solution": "solution_marker_gamma",
                    "affected_files": []
                }"#,
            )
            .unwrap(),
        );

        // 更新统计字段(会触发 decisions_au)
        log.verify_decision(id, DecisionVerification::Confirmed, None)
            .unwrap();

        // 验证 FTS5 索引仍可检索(说明 trigger 没有破坏索引)
        let result = log.search_decisions("unique_marker_alpha", 10).unwrap();
        assert!(
            result.contains("unique_marker_alpha"),
            "FTS5 search after verify_decision should still find the record, got: {result}"
        );
        // 同时验证 success_rate 在 search 结果中显示为新值
        assert!(
            result.contains("100%"),
            "search should display updated success_rate=100%, got: {result}"
        );
    }

    #[test]
    fn verify_decision_multiple_mixed_sequence_matches_running_mean() {
        // 模拟真实场景:Confuted → Refuted → Confirmed → Confirmed → Partial
        // 期望 success_rate 等价于对 [1.0, 0.0, 1.0, 1.0, 0.5] 求 running mean。
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let id = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "verify-test",
                    "problem_signature": "mixed sequence running mean",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": []
                }"#,
            )
            .unwrap(),
        );

        // 预期序列: rate = (0*0+1)/1=1.0, (1*1+0)/2=0.5, (0.5*2+1)/3=2/3,
        //           (2/3*3+1)/4 = 0.75, (0.75*4+0.5)/5 = 0.7
        let sequence = [
            (DecisionVerification::Confirmed, 1.0_f64),
            (DecisionVerification::Refuted, 0.5),
            (DecisionVerification::Confirmed, 2.0 / 3.0),
            (DecisionVerification::Confirmed, 0.75),
            (DecisionVerification::Partial, 0.7),
        ];

        let mut expected_count = 0_i64;
        for (i, (v, expected_rate)) in sequence.iter().enumerate() {
            let out = log.verify_decision(id, *v, None).unwrap();
            expected_count += 1;
            let got_count = parse_verify_count(&out);
            let got_rate = parse_success_rate(&out);
            assert_eq!(got_count, expected_count, "step {i}: count mismatch");
            // {:.6} 格式精度上限误差为 5e-7,1e-5 提供 20x 安全裕度。
            assert!(
                (got_rate - expected_rate).abs() < 1e-5,
                "step {i}: expected rate {expected_rate}, got {got_rate} (full: {out})"
            );
        }
    }

    #[test]
    fn verify_decision_transaction_rollback_on_missing_id() {
        // 验证 BEGIN IMMEDIATE + ROLLBACK 路径:对不存在的 id 调用后,
        // 后续操作(包括新的 log_decision)应能正常执行,
        // 说明连接没卡在事务中。
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        // 先创建一条决策,后续会用它来确认连接仍可用
        let id1 = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "rollback-test",
                    "problem_signature": "first decision",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": []
                }"#,
            )
            .unwrap(),
        );

        // 触发错误路径(不存在的 id)
        let _ = log
            .verify_decision(999999, DecisionVerification::Confirmed, None)
            .unwrap_err();

        // 连接应该已经 ROLLBACK,可以继续正常操作
        let out = log
            .verify_decision(id1, DecisionVerification::Confirmed, None)
            .unwrap();
        assert!(
            out.contains("result=Confirmed"),
            "post-rollback verify should succeed, got {out}"
        );

        // 同时验证可以继续插入新决策
        let id2 = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "rollback-test",
                    "problem_signature": "second decision post-rollback",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": []
                }"#,
            )
            .unwrap(),
        );
        assert_ne!(id1, id2, "new decision should get a fresh id");
    }

    #[test]
    fn verify_decision_clamps_floating_point_drift() {
        // 大量 Confirmed 后 success_rate 应严格在 [0,1],
        // 不会有浮点误差导致 > 1.0 的情况。
        let dir = tempfile::tempdir().unwrap();
        let log = DecisionLog::open(dir.path()).unwrap();

        let id = parse_decision_id(
            &log.log_decision(
                r#"{
                    "session_id": "clamp-test",
                    "problem_signature": "clamp",
                    "root_cause_hypothesis": "h",
                    "applied_solution": "s",
                    "affected_files": []
                }"#,
            )
            .unwrap(),
        );

        for _ in 0..50 {
            log.verify_decision(id, DecisionVerification::Confirmed, None)
                .unwrap();
        }
        // 50 次 Confirmed 后 rate 应该 = 1.0(不会因浮点变成 1.0000000001)
        let conn = log.conn.lock().unwrap();
        let rate: f64 = conn
            .query_row(
                "SELECT success_rate FROM decisions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert!(
            rate <= 1.0,
            "rate should not exceed 1.0 (clamp), got {rate}"
        );
        assert!(
            (rate - 1.0).abs() < 1e-9,
            "rate should be exactly 1.0 after 50 Confirmed, got {rate}"
        );
    }

    // -----------------------------------------------------------------
    // §4.7 DecisionPoint / extract_decisions_before_compaction 测试
    // -----------------------------------------------------------------

    #[test]
    fn detect_decision_signal_matches_english_keywords() {
        assert!(detect_decision_signal(
            "we decided to use tokio for async runtime"
        ));
        assert!(detect_decision_signal(
            "I chose the flash model for budget tasks"
        ));
        assert!(detect_decision_signal(
            "trade-off between latency and throughput"
        ));
        assert!(detect_decision_signal("rejected the alternative approach"));
        assert!(detect_decision_signal("ruled out option C"));
        assert!(detect_decision_signal("used A instead of B"));
        assert!(detect_decision_signal("picked A rather than B"));
    }

    #[test]
    fn detect_decision_signal_matches_chinese_keywords() {
        assert!(detect_decision_signal("我们决定使用 tokio"));
        assert!(detect_decision_signal("选择 flash 模型"));
        assert!(detect_decision_signal("权衡延迟与吞吐"));
        assert!(detect_decision_signal("否决了备选方案"));
        assert!(detect_decision_signal("放弃原计划"));
        assert!(detect_decision_signal("之所以这样设计,是因为向后兼容"));
        assert!(detect_decision_signal("用 A 而非 B"));
        assert!(detect_decision_signal("用 A 而不是 B"));
    }

    #[test]
    fn detect_decision_signal_no_match_for_plain_text() {
        assert!(!detect_decision_signal("hello world"));
        assert!(!detect_decision_signal(
            "the quick brown fox jumps over the lazy dog"
        ));
        assert!(!detect_decision_signal("这是一段普通文本,没有决策关键词"));
        assert!(!detect_decision_signal(""));
    }

    #[test]
    fn detect_decision_signal_case_insensitive() {
        assert!(detect_decision_signal("We DECIDED to..."));
        assert!(detect_decision_signal("TRADE-OFF considered"));
        assert!(detect_decision_signal("Chose this option"));
    }

    #[test]
    fn extract_decisions_heuristic_finds_decision_messages() {
        let messages = vec![
            "let's discuss the architecture",
            "we decided to use SQLite for persistence", // 命中 "decided"
            "the implementation plan is...",
            "chose flash model for budget tier", // 命中 "chose"
            "no decision here, just facts",
        ];
        let decisions = extract_decisions_before_compaction(
            &messages,
            &DetectionStrategy::Heuristic,
            "test-session",
        );
        assert_eq!(decisions.len(), 2, "should find 2 decision messages");
        // 验证 ID 格式
        assert!(decisions[0].id.starts_with("d"));
        assert!(decisions[1].id.starts_with("d"));
        // 验证 session_id 透传
        assert_eq!(decisions[0].session_id, "test-session");
        assert_eq!(decisions[1].session_id, "test-session");
        // 验证上下文提取(前一条消息)
        assert_eq!(decisions[0].context, "let's discuss the architecture");
        assert_eq!(decisions[1].context, "the implementation plan is...");
        // 验证 decision/rationale 字段填充
        assert!(decisions[0].decision.contains("SQLite"));
        assert!(decisions[0].rationale.contains("SQLite"));
        // MVP 不提取 alternatives
        assert!(decisions[0].alternatives.is_empty());
    }

    #[test]
    fn extract_decisions_heuristic_first_message_has_empty_context() {
        // 第一条消息命中关键词时,context 应为空字符串
        let messages = vec!["decided to use Rust for the CLI"];
        let decisions =
            extract_decisions_before_compaction(&messages, &DetectionStrategy::Heuristic, "sess");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].context, "");
    }

    #[test]
    fn extract_decisions_heuristic_empty_input() {
        let messages: Vec<&str> = vec![];
        let decisions =
            extract_decisions_before_compaction(&messages, &DetectionStrategy::Heuristic, "sess");
        assert!(decisions.is_empty());
    }

    #[test]
    fn extract_decisions_heuristic_no_keywords_no_decisions() {
        let messages = vec!["hello world", "the quick brown fox", "just some plain text"];
        let decisions =
            extract_decisions_before_compaction(&messages, &DetectionStrategy::Heuristic, "sess");
        assert!(decisions.is_empty());
    }

    #[test]
    fn extract_decisions_llm_extract_falls_back_to_heuristic_without_client() {
        // v2 §10.5 Epic 6:无全局 client 时,LlmExtract 降级为 Heuristic
        // "decided" 是启发式关键词,应被检测到
        let messages = vec!["we decided to use Rust"];
        let decisions = extract_decisions_before_compaction(
            &messages,
            &DetectionStrategy::LlmExtract {
                model: "deepseek-v4-flash".to_string(),
            },
            "sess",
        );
        // 降级为 Heuristic 后应检测到 "decided" 关键词
        assert_eq!(
            decisions.len(),
            1,
            "LlmExtract 无 client 时应降级为 Heuristic 并检测到 'decided'"
        );
        assert!(decisions[0].decision.contains("Rust"));
    }

    #[test]
    fn extract_decisions_truncates_long_messages() {
        // 构造超长消息,验证截断逻辑
        let long_msg = "decided: ".to_string() + &"x".repeat(1000);
        let messages = vec![long_msg.as_str()];
        let decisions =
            extract_decisions_before_compaction(&messages, &DetectionStrategy::Heuristic, "sess");
        assert_eq!(decisions.len(), 1);
        // decision 字段截断到 300 字节 + "…"(3 字节 UTF-8)= 303 字节
        assert!(
            decisions[0].decision.len() <= 303,
            "decision len {} should be <= 303",
            decisions[0].decision.len()
        );
        // rationale 字段截断到 500 字节 + "…"(3 字节 UTF-8)= 503 字节
        assert!(
            decisions[0].rationale.len() <= 503,
            "rationale len {} should be <= 503",
            decisions[0].rationale.len()
        );
    }

    #[test]
    fn truncate_for_decision_handles_multibyte_utf8() {
        // ASCII:正常截断
        let r = truncate_for_decision("hello world", 5);
        assert_eq!(r, "hello…");
        // 不需要截断
        assert_eq!(truncate_for_decision("short", 200), "short");
        // 中文:3 字节字符,max 落在字符中间不应 panic
        let chinese = "你好世界测试字符串";
        let r = truncate_for_decision(chinese, 7); // 7 落在第二个中文字符(字节 3-5)的中间
        assert!(r.ends_with('…'));
        assert!(!r.is_empty());
        // 结果应该是 "你好…"(截到字节 6,即第二个完整中文字符之后)
        assert_eq!(r, "你好…");
        // max = 0 的极端情况
        assert_eq!(truncate_for_decision("hello", 0), "…");
    }

    #[test]
    fn render_decision_for_notebook_empty_returns_empty() {
        let decisions: Vec<DecisionPoint> = vec![];
        assert_eq!(render_decision_for_notebook(&decisions), "");
    }

    #[test]
    fn render_decision_for_notebook_renders_decision_lines() {
        let decisions = vec![DecisionPoint {
            id: "d1234567890-0".to_string(),
            context: "ctx".to_string(),
            decision: "use SQLite".to_string(),
            rationale: "simpler than Postgres".to_string(),
            alternatives: vec![],
            timestamp_ms: 1234567890,
            session_id: "sess".to_string(),
        }];
        let rendered = render_decision_for_notebook(&decisions);
        assert!(rendered.contains("[d1234567]"));
        assert!(rendered.contains("use SQLite"));
        assert!(rendered.contains("simpler than Postgres"));
    }

    #[test]
    fn render_decision_for_notebook_includes_alternatives() {
        let decisions = vec![DecisionPoint {
            id: "d1-0".to_string(),
            context: "ctx".to_string(),
            decision: "use Rust".to_string(),
            rationale: "performance".to_string(),
            alternatives: vec!["Go".to_string(), "C++".to_string()],
            timestamp_ms: 1,
            session_id: "sess".to_string(),
        }];
        let rendered = render_decision_for_notebook(&decisions);
        assert!(rendered.contains("alternatives: Go; C++"));
    }

    #[test]
    fn render_decision_for_notebook_caps_at_20_decisions() {
        let decisions: Vec<DecisionPoint> = (0..50)
            .map(|i| DecisionPoint {
                id: format!("d-{i}"),
                context: "ctx".to_string(),
                decision: format!("decision {i}"),
                rationale: "why".to_string(),
                alternatives: vec![],
                timestamp_ms: i as u64,
                session_id: "sess".to_string(),
            })
            .collect();
        let rendered = render_decision_for_notebook(&decisions);
        // 应该只渲染前 20 条
        let line_count = rendered.lines().count();
        assert_eq!(line_count, 20, "should cap at 20 decisions");
        assert!(rendered.contains("decision 0"));
        assert!(rendered.contains("decision 19"));
        assert!(!rendered.contains("decision 20"));
    }

    #[test]
    fn persist_decisions_to_notebook_empty_decisions_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let decisions: Vec<DecisionPoint> = vec![];
        let result = persist_decisions_to_notebook(dir.path(), &decisions);
        assert!(result.is_ok());
        // NOTEBOOK.md 不应被创建(空输入直接返回)
        let notebook_path = dir.path().join(".claw/NOTEBOOK.md");
        assert!(!notebook_path.exists());
    }

    #[test]
    fn persist_decisions_to_notebook_creates_decisions_section() {
        let dir = tempfile::tempdir().unwrap();
        let decisions = vec![DecisionPoint {
            id: "d123-0".to_string(),
            context: "discussing async runtime".to_string(),
            decision: "use tokio".to_string(),
            rationale: "industry standard".to_string(),
            alternatives: vec![],
            timestamp_ms: 123,
            session_id: "sess".to_string(),
        }];
        let result = persist_decisions_to_notebook(dir.path(), &decisions);
        assert!(result.is_ok(), "persist failed: {:?}", result.err());
        // 加载 NOTEBOOK 验证 decisions 段
        let notebook = crate::notebook::Notebook::load(dir.path()).unwrap();
        let decisions_section = notebook.get_section("decisions");
        assert!(
            decisions_section.is_some(),
            "decisions section should exist"
        );
        let content = decisions_section.unwrap();
        assert!(content.contains("use tokio"));
        assert!(content.contains("industry standard"));
    }

    #[test]
    fn persist_decisions_to_notebook_appends_to_existing() {
        let dir = tempfile::tempdir().unwrap();
        // 第一次写入
        let decisions1 = vec![DecisionPoint {
            id: "d1-0".to_string(),
            context: "ctx1".to_string(),
            decision: "first decision".to_string(),
            rationale: "why1".to_string(),
            alternatives: vec![],
            timestamp_ms: 1,
            session_id: "sess".to_string(),
        }];
        persist_decisions_to_notebook(dir.path(), &decisions1).unwrap();
        // 第二次写入
        let decisions2 = vec![DecisionPoint {
            id: "d2-0".to_string(),
            context: "ctx2".to_string(),
            decision: "second decision".to_string(),
            rationale: "why2".to_string(),
            alternatives: vec![],
            timestamp_ms: 2,
            session_id: "sess".to_string(),
        }];
        persist_decisions_to_notebook(dir.path(), &decisions2).unwrap();
        // 验证累积(两条决策都在)
        let notebook = crate::notebook::Notebook::load(dir.path()).unwrap();
        let content = notebook.get_section("decisions").unwrap();
        assert!(content.contains("first decision"), "first decision missing");
        assert!(
            content.contains("second decision"),
            "second decision missing"
        );
    }

    #[test]
    fn persist_decisions_to_notebook_trims_when_exceeding_limit() {
        let dir = tempfile::tempdir().unwrap();
        // 构造 200 条决策,每条较长,确保超过 14_000 字符上限
        let decisions: Vec<DecisionPoint> = (0..200)
            .map(|i| DecisionPoint {
                id: format!("d-{i}"),
                context: format!("context for decision {i}"),
                decision: format!("decision {i}: use approach {}", i % 5),
                rationale: format!("rationale {i}: {}", "x".repeat(50)),
                alternatives: vec![],
                timestamp_ms: i as u64,
                session_id: "sess".to_string(),
            })
            .collect();
        let result = persist_decisions_to_notebook(dir.path(), &decisions);
        assert!(result.is_ok(), "trim path failed: {:?}", result.err());
        // 验证 NOTEBOOK 仍可加载且 decisions 段被截断到 100 行以内
        let notebook = crate::notebook::Notebook::load(dir.path()).unwrap();
        let content = notebook.get_section("decisions").unwrap();
        let line_count = content.lines().count();
        assert!(
            line_count <= 100,
            "decisions section should be trimmed to <= 100 lines, got {line_count}"
        );
    }

    #[test]
    fn decision_point_serializes_to_json() {
        // 验证 serde 序列化/反序列化 round-trip
        let dp = DecisionPoint {
            id: "d123-0".to_string(),
            context: "ctx".to_string(),
            decision: "decide".to_string(),
            rationale: "why".to_string(),
            alternatives: vec!["alt1".to_string()],
            timestamp_ms: 123,
            session_id: "sess".to_string(),
        };
        let json = serde_json::to_string(&dp).unwrap();
        let parsed: DecisionPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, dp.id);
        assert_eq!(parsed.context, dp.context);
        assert_eq!(parsed.decision, dp.decision);
        assert_eq!(parsed.rationale, dp.rationale);
        assert_eq!(parsed.alternatives, dp.alternatives);
        assert_eq!(parsed.timestamp_ms, dp.timestamp_ms);
        assert_eq!(parsed.session_id, dp.session_id);
    }

    // ===== v2 §10.5 Epic 6:LlmExtract 决策提取测试 =====

    /// mock 决策提取 client — 仅用于测试。
    struct MockDecisionExtractorClient {
        response: String,
        force_error: bool,
    }

    impl DecisionExtractorClient for MockDecisionExtractorClient {
        fn extract(&self, _prompt: &str) -> Result<String, String> {
            if self.force_error {
                return Err("mock API failure".to_string());
            }
            Ok(self.response.clone())
        }
    }

    /// §10.5 Epic 6:parse_llm_decision_json 解析标准 JSON 数组
    #[test]
    fn parse_llm_decision_json_parses_standard_array() {
        let response = r#"```json
[
  {
    "context": "语言选型",
    "decision": "选择 Rust",
    "rationale": "性能更好且内存安全",
    "alternatives": ["Go", "Python"]
  }
]
```"#;
        let decisions = parse_llm_decision_json(response, 1000, "sess").unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].context, "语言选型");
        assert_eq!(decisions[0].decision, "选择 Rust");
        assert_eq!(decisions[0].rationale, "性能更好且内存安全");
        assert_eq!(decisions[0].alternatives, vec!["Go", "Python"]);
        assert_eq!(decisions[0].session_id, "sess");
        assert_eq!(decisions[0].timestamp_ms, 1000);
        assert!(decisions[0].id.starts_with("d1000-llm-"));
    }

    /// §10.5 Epic 6:parse_llm_decision_json 解析无 markdown 包裹的 JSON
    #[test]
    fn parse_llm_decision_json_parses_plain_json() {
        let response =
            r#"[{"context":"ctx","decision":"decide","rationale":"why","alternatives":[]}]"#;
        let decisions = parse_llm_decision_json(response, 2000, "sess").unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, "decide");
    }

    /// §10.5 Epic 6:parse_llm_decision_json 解析多条决策
    #[test]
    fn parse_llm_decision_json_parses_multiple_decisions() {
        let response = r#"[
          {"context":"c1","decision":"d1","rationale":"r1","alternatives":[]},
          {"context":"c2","decision":"d2","rationale":"r2","alternatives":["a"]}
        ]"#;
        let decisions = parse_llm_decision_json(response, 3000, "sess").unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].decision, "d1");
        assert_eq!(decisions[1].decision, "d2");
        assert_eq!(decisions[1].alternatives, vec!["a"]);
    }

    /// §10.5 Epic 6:parse_llm_decision_json 跳过 decision 为空的条目
    #[test]
    fn parse_llm_decision_json_skips_empty_decision() {
        let response = r#"[
          {"context":"c1","decision":"","rationale":"r1","alternatives":[]},
          {"context":"c2","decision":"d2","rationale":"r2","alternatives":[]}
        ]"#;
        let decisions = parse_llm_decision_json(response, 4000, "sess").unwrap();
        assert_eq!(decisions.len(), 1, "空 decision 条目应被跳过");
        assert_eq!(decisions[0].decision, "d2");
    }

    /// §10.5 Epic 6:parse_llm_decision_json 对非法 JSON 返回 Err
    #[test]
    fn parse_llm_decision_json_errors_for_invalid_json() {
        let response = "not json at all";
        let result = parse_llm_decision_json(response, 5000, "sess");
        assert!(result.is_err(), "无 JSON 数组应返回 Err");
    }

    /// §10.5 Epic 6:parse_llm_decision_json 容错 — 字段缺失时用默认空字符串
    #[test]
    fn parse_llm_decision_json_tolerates_missing_fields() {
        // 仅 decision 字段,其余缺失
        let response = r#"[{"decision":"only decision"}]"#;
        let decisions = parse_llm_decision_json(response, 6000, "sess").unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, "only decision");
        assert_eq!(decisions[0].context, "", "缺失字段应为空字符串");
        assert_eq!(decisions[0].rationale, "");
        assert!(decisions[0].alternatives.is_empty());
    }

    /// §10.5 Epic 6:parse_llm_decision_json 截断超长字段
    #[test]
    fn parse_llm_decision_json_truncates_long_fields() {
        let long_context = "x".repeat(300);
        let long_decision = "d".repeat(400);
        let response = format!(
            r#"[{{"context":"{long_context}","decision":"{long_decision}","rationale":"r","alternatives":[]}}]"#
        );
        let decisions = parse_llm_decision_json(&response, 7000, "sess").unwrap();
        assert_eq!(decisions.len(), 1);
        // context 截断到 200 字符 + 省略号(UTF-8 中 … 是 3 字节)
        // 原文 300 字符,截断后应明显变短(≤ 203 字节 = 200 字符 + 3 字节省略号)
        assert!(
            decisions[0].context.chars().count() <= 201,
            "context 应被截断到 ≤201 字符, got {} chars",
            decisions[0].context.chars().count()
        );
        assert!(decisions[0].context.ends_with('…'));
        // decision 截断到 300 字符 + 省略号
        assert!(
            decisions[0].decision.chars().count() <= 301,
            "decision 应被截断到 ≤301 字符, got {} chars",
            decisions[0].decision.chars().count()
        );
        assert!(decisions[0].decision.ends_with('…'));
    }

    /// §10.5 Epic 6:extract_decisions_with_llm 成功提取决策
    #[test]
    fn extract_decisions_with_llm_extracts_from_mock_client() {
        let messages = vec!["we discussed the architecture"];
        let json_response = serde_json::json!([{
            "context": "架构讨论",
            "decision": "采用微服务",
            "rationale": "可扩展性",
            "alternatives": ["单体"]
        }])
        .to_string();
        let mock = MockDecisionExtractorClient {
            response: json_response,
            force_error: false,
        };
        let decisions = extract_decisions_with_llm(&messages, "flash", &mock, "sess");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, "采用微服务");
        assert_eq!(decisions[0].alternatives, vec!["单体"]);
        assert!(
            decisions[0].id.contains("llm"),
            "LLM 提取的 id 应含 'llm' 标记"
        );
    }

    /// §10.5 Epic 6:extract_decisions_with_llm 在 API 失败时降级为 Heuristic
    #[test]
    fn extract_decisions_with_llm_falls_back_on_api_failure() {
        // "decided" 是启发式关键词,降级后应被检测到
        let messages = vec!["we decided to use Rust"];
        let mock = MockDecisionExtractorClient {
            response: String::new(),
            force_error: true,
        };
        let decisions = extract_decisions_with_llm(&messages, "flash", &mock, "sess");
        assert_eq!(
            decisions.len(),
            1,
            "API 失败应降级为 Heuristic 并检测到 'decided'"
        );
        assert!(decisions[0].decision.contains("Rust"));
        // Heuristic 的 id 不含 'llm' 标记
        assert!(!decisions[0].id.contains("llm"));
    }

    /// §10.5 Epic 6:extract_decisions_with_llm 在 JSON 解析失败时降级为 Heuristic
    #[test]
    fn extract_decisions_with_llm_falls_back_on_json_parse_failure() {
        let messages = vec!["we decided to use Rust"];
        let mock = MockDecisionExtractorClient {
            response: "invalid response not json".to_string(),
            force_error: false,
        };
        let decisions = extract_decisions_with_llm(&messages, "flash", &mock, "sess");
        assert_eq!(decisions.len(), 1, "JSON 解析失败应降级为 Heuristic");
    }

    /// §10.5 Epic 6:extract_decisions_with_llm 在 LLM 返回空数组时降级为 Heuristic
    #[test]
    fn extract_decisions_with_llm_falls_back_on_empty_response() {
        let messages = vec!["we decided to use Rust"];
        let mock = MockDecisionExtractorClient {
            response: "[]".to_string(),
            force_error: false,
        };
        let decisions = extract_decisions_with_llm(&messages, "flash", &mock, "sess");
        assert_eq!(decisions.len(), 1, "LLM 返回空数组应降级为 Heuristic");
    }

    /// §10.5 Epic 6:build_llm_extract_prompt 包含消息列表和 JSON schema
    #[test]
    fn build_llm_extract_prompt_contains_required_sections() {
        let messages = vec!["msg1", "msg2"];
        let prompt = build_llm_extract_prompt(&messages);
        assert!(prompt.contains("[0] msg1"), "prompt 应含消息 [0]");
        assert!(prompt.contains("[1] msg2"), "prompt 应含消息 [1]");
        assert!(prompt.contains("设计决策点"), "prompt 应含判定标准");
        assert!(prompt.contains("JSON"), "prompt 应含 JSON schema");
        assert!(
            prompt.contains("alternatives"),
            "prompt 应含 alternatives 字段说明"
        );
        assert!(prompt.contains("few-shot"), "prompt 应含 few-shot 示例");
    }

    /// §10.5 Epic 6:strip_markdown_code_block 剥离 ```json ... ``` 包裹
    #[test]
    fn strip_markdown_code_block_handles_json_block() {
        let input = "```json\n[{\"a\":1}]\n```";
        let stripped = strip_markdown_code_block(input);
        assert_eq!(stripped, "[{\"a\":1}]");
    }

    /// §10.5 Epic 6:strip_markdown_code_block 剥离无语言标识的 ``` ... ``` 包裹
    #[test]
    fn strip_markdown_code_block_handles_plain_block() {
        let input = "```\n[{\"a\":1}]\n```";
        let stripped = strip_markdown_code_block(input);
        assert_eq!(stripped, "[{\"a\":1}]");
    }

    /// §10.5 Epic 6:strip_markdown_code_block 对无包裹的文本原样返回
    #[test]
    fn strip_markdown_code_block_passes_through_plain_text() {
        let input = "[{\"a\":1}]";
        let stripped = strip_markdown_code_block(input);
        assert_eq!(stripped, "[{\"a\":1}]");
    }
}
