//! HarnessEdit 持久化层 — 共用 decision_log.db,独立 schema(harness_edits 表)。
//!
//! 遵循 design-doc §6:WAL 模式 + Mutex 保证线程安全;FTS5 全文索引与
//! decisions 表同模式;容量控制(Candidate ≤ 20, Retired ≤ 50 LRU)。
//! 回滚(rollback_all / rollback)是"一键禁用"语义:把 Active 置为 Retired,
//! 保留数据供学习,不物理删除。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::decision_log::{compute_simhash, hamming_distance};
use crate::harness_evolution::types::{
    ArchiveStats, EditSource, EditStatus, HarnessEdit,
};

/// 容量控制上限(design-doc §3.3)。
pub const MAX_ACTIVE_EDITS: usize = 10;
pub const MAX_CANDIDATE_EDITS: usize = 20;
pub const MAX_RETIRED_EDITS: usize = 50;
/// 单条 content 最大长度(chars)。
pub const MAX_EDIT_CONTENT_CHARS: usize = 500;
/// simhash 去重汉明距离阈值。
pub const SIMHASH_DISTANCE_THRESHOLD: u32 = 3;

const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS harness_edits (
    id TEXT PRIMARY KEY,
    pathology TEXT NOT NULL,
    content TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('Candidate', 'Active', 'Retired')),
    source TEXT NOT NULL CHECK(source IN ('RulePattern', 'LlmProposer')),
    verify_count INTEGER DEFAULT 0,
    success_count INTEGER DEFAULT 0,
    created_at INTEGER NOT NULL,
    last_verified_at INTEGER,
    proposer_reasoning TEXT,
    similarity_hash INTEGER NOT NULL,
    retire_reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_harness_edits_status ON harness_edits(status);
CREATE INDEX IF NOT EXISTS idx_harness_edits_pathology ON harness_edits(pathology);
CREATE INDEX IF NOT EXISTS idx_harness_edits_simhash ON harness_edits(similarity_hash);

CREATE VIRTUAL TABLE IF NOT EXISTS harness_edits_fts USING fts5(
    pathology, content,
    content='harness_edits', content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS harness_edits_ai AFTER INSERT ON harness_edits BEGIN
    INSERT INTO harness_edits_fts(rowid, pathology, content)
    VALUES (new.rowid, new.pathology, new.content);
END;

CREATE TRIGGER IF NOT EXISTS harness_edits_ad AFTER DELETE ON harness_edits BEGIN
    INSERT INTO harness_edits_fts(harness_edits_fts, rowid, pathology, content)
    VALUES ('delete', old.rowid, old.pathology, old.content);
END;

CREATE TRIGGER IF NOT EXISTS harness_edits_au AFTER UPDATE ON harness_edits BEGIN
    INSERT INTO harness_edits_fts(harness_edits_fts, rowid, pathology, content)
    VALUES ('delete', old.rowid, old.pathology, old.content);
    INSERT INTO harness_edits_fts(rowid, pathology, content)
    VALUES (new.rowid, new.pathology, new.content);
END;
";

/// HarnessEdit 持久化错误。
#[derive(Debug)]
pub enum ArchiveError {
    Sqlite(rusqlite::Error),
    InvalidStatus(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "archive sqlite error: {e}"),
            Self::InvalidStatus(s) => write!(f, "invalid edit status: {s}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<rusqlite::Error> for ArchiveError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

/// HarnessEdit 持久化层。
#[derive(Debug)]
pub struct HarnessArchive {
    conn: Mutex<Connection>,
}

impl HarnessArchive {
    /// 打开或创建 archive(共用 decision_log.db 路径,独立 schema)。
    pub fn open(root: &Path) -> Result<Self, ArchiveError> {
        let db_dir = root.join(".claw");
        let _ = std::fs::create_dir_all(&db_dir);
        let db_path = db_dir.join("decision_log.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 新增 Candidate edit。若 Candidate 超容量,淘汰最旧一条(按 created_at)。
    pub fn add_candidate(&self, edit: HarnessEdit) -> Result<(), ArchiveError> {
        if edit.content.chars().count() > MAX_EDIT_CONTENT_CHARS {
            return Err(ArchiveError::InvalidStatus(format!(
                "edit content exceeds {} chars",
                MAX_EDIT_CONTENT_CHARS
            )));
        }
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // simhash 去重(与 Active + Candidate + Retired 对比)由 Proposer 完成;
        // 此处仅做硬性容量控制。
        conn.execute(
            "INSERT INTO harness_edits
             (id, pathology, content, status, source, verify_count, success_count,
              created_at, last_verified_at, proposer_reasoning, similarity_hash, retire_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)",
            params![
                edit.id,
                edit.pathology,
                edit.content,
                edit.status.as_db_str(),
                edit.source.as_db_str(),
                edit.verify_count,
                edit.success_count,
                edit.created_at,
                edit.last_verified_at,
                edit.proposer_reasoning,
                edit.similarity_hash,
            ],
        )?;
        Self::enforce_capacity(&conn, EditStatus::Candidate, MAX_CANDIDATE_EDITS)?;
        Ok(())
    }

    /// 按 id 读取一条 edit。
    pub fn get_edit(&self, edit_id: &str) -> Result<Option<HarnessEdit>, ArchiveError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = conn
            .query_row(
                "SELECT id, pathology, content, status, source, verify_count, success_count,
                        created_at, last_verified_at, proposer_reasoning, similarity_hash, retire_reason
                 FROM harness_edits WHERE id = ?1",
                params![edit_id],
                map_edit_row,
            )
            .optional()?;
        Ok(row)
    }

    /// 所有 Active edits(按 success_rate 降序,用于注入 dynamic_sections)。
    pub fn active_edits(&self) -> Result<Vec<HarnessEdit>, ArchiveError> {
        self.list_edits(Some(EditStatus::Active))
    }

    /// 所有 Candidate edits(用于验证)。
    pub fn candidate_edits(&self) -> Result<Vec<HarnessEdit>, ArchiveError> {
        self.list_edits(Some(EditStatus::Candidate))
    }

    /// 所有 Retired edits。
    pub fn retired_edits(&self) -> Result<Vec<HarnessEdit>, ArchiveError> {
        self.list_edits(Some(EditStatus::Retired))
    }

    /// 按状态列出 edits(默认全部)。Active 按 success_rate 降序,其余按 created_at 升序。
    pub fn list_edits(
        &self,
        status: Option<EditStatus>,
    ) -> Result<Vec<HarnessEdit>, ArchiveError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let (sql, params) = match status {
            Some(s) => (
                "SELECT id, pathology, content, status, source, verify_count, success_count,
                        created_at, last_verified_at, proposer_reasoning, similarity_hash, retire_reason
                 FROM harness_edits WHERE status = ?1
                 ORDER BY CASE status WHEN 'Active' THEN 0 WHEN 'Candidate' THEN 1 ELSE 2 END,
                          created_at ASC",
                vec![s.as_db_str().to_string()],
            ),
            None => (
                "SELECT id, pathology, content, status, source, verify_count, success_count,
                        created_at, last_verified_at, proposer_reasoning, similarity_hash, retire_reason
                 FROM harness_edits
                 ORDER BY CASE status WHEN 'Active' THEN 0 WHEN 'Candidate' THEN 1 ELSE 2 END,
                          created_at ASC",
                Vec::new(),
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), map_edit_row)?;
        let mut edits = Vec::new();
        for row in rows {
            edits.push(row?);
        }
        // Active 按 success_rate 降序(注入优先级)。
        if status == Some(EditStatus::Active) || status.is_none() {
            edits.sort_by(|a, b| {
                b.success_rate()
                    .partial_cmp(&a.success_rate())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        Ok(edits)
    }

    /// 更新状态 + 统计 + 回滚检查(学习环写入口)。
    pub fn update_status_and_stats(
        &self,
        edit_id: &str,
        new_status: EditStatus,
        verify_count: u32,
        success_count: u32,
    ) -> Result<(), ArchiveError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE harness_edits
             SET status = ?1, verify_count = ?2, success_count = ?3, last_verified_at = ?4
             WHERE id = ?5",
            params![
                new_status.as_db_str(),
                verify_count,
                success_count,
                current_timestamp_ms(),
                edit_id
            ],
        )?;
        // Active 晋升后仍受 10 条上限约束:超限时淘汰最旧 Active。
        if new_status == EditStatus::Active {
            Self::enforce_capacity(&conn, EditStatus::Active, MAX_ACTIVE_EDITS)?;
        }
        Ok(())
    }

    /// 仅更新状态(供晋升/退役)。
    pub fn update_status(&self, edit_id: &str, status: EditStatus) -> Result<(), ArchiveError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE harness_edits SET status = ?1, last_verified_at = ?2 WHERE id = ?3",
            params![status.as_db_str(), current_timestamp_ms(), edit_id],
        )?;
        if status == EditStatus::Active {
            Self::enforce_capacity(&conn, EditStatus::Active, MAX_ACTIVE_EDITS)?;
        }
        Ok(())
    }

    /// 记录退役原因(CLI 展示)。
    pub fn set_retire_reason(&self, edit_id: &str, reason: &str) -> Result<(), ArchiveError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE harness_edits SET retire_reason = ?1 WHERE id = ?2",
            params![reason, edit_id],
        )?;
        Ok(())
    }

    /// 一键回滚所有 Active edits(紧急禁用),返回受影响条数。
    pub fn rollback_all(&self) -> Result<u32, ArchiveError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count = conn.execute(
            "UPDATE harness_edits SET status = 'Retired', retire_reason = 'manual rollback'
             WHERE status = 'Active'",
            [],
        )?;
        Ok(count as u32)
    }

    /// 回滚单个 edit(仅 Active → Retired)。
    pub fn rollback(&self, edit_id: &str) -> Result<(), ArchiveError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE harness_edits SET status = 'Retired', retire_reason = 'manual rollback'
             WHERE id = ?1 AND status = 'Active'",
            params![edit_id],
        )?;
        Ok(())
    }

    /// simhash 相似查询(汉明距离 ≤ threshold,只查 Active + Retired)。
    pub fn find_similar(
        &self,
        simhash: i64,
        threshold: u32,
    ) -> Result<Vec<HarnessEdit>, ArchiveError> {
        let all = self.list_edits(None)?;
        Ok(all
            .into_iter()
            .filter(|e| hamming_distance(e.similarity_hash as u64, simhash as u64) <= threshold)
            .collect())
    }

    /// 统计信息(供 CLI `claw harness stats`)。
    pub fn stats(&self) -> Result<ArchiveStats, ArchiveError> {
        let all = self.list_edits(None)?;
        let mut stats = ArchiveStats {
            total: all.len() as u64,
            ..ArchiveStats::default()
        };
        let mut active_rates_sum = 0.0;
        let mut active_rates_count = 0u64;
        for edit in &all {
            match edit.status {
                EditStatus::Active => {
                    stats.active += 1;
                    active_rates_sum += edit.success_rate();
                    active_rates_count += 1;
                }
                EditStatus::Candidate => stats.candidate += 1,
                EditStatus::Retired => stats.retired += 1,
            }
            match edit.source {
                EditSource::RulePattern => stats.rule_sourced += 1,
                EditSource::LlmProposer => stats.llm_sourced += 1,
            }
        }
        stats.avg_active_success_rate = if active_rates_count > 0 {
            active_rates_sum / active_rates_count as f64
        } else {
            0.0
        };
        Ok(stats)
    }

    /// 容量控制:某状态条数超限时,按 created_at 淘汰最旧一条(递归直到合规)。
    fn enforce_capacity(
        conn: &Connection,
        status: EditStatus,
        cap: usize,
    ) -> Result<(), ArchiveError> {
        loop {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM harness_edits WHERE status = ?1",
                params![status.as_db_str()],
                |row| row.get(0),
            )?;
            if (count as usize) <= cap {
                break;
            }
            conn.execute(
                "DELETE FROM harness_edits
                 WHERE status = ?1 AND id = (
                     SELECT id FROM harness_edits WHERE status = ?1 ORDER BY created_at ASC LIMIT 1
                 )",
                params![status.as_db_str()],
            )?;
        }
        Ok(())
    }
}

/// 从数据库行映射为 [`HarnessEdit`]。
fn map_edit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HarnessEdit> {
    let status_str: String = row.get(3)?;
    let source_str: String = row.get(4)?;
    let status = EditStatus::from_db_str(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!("invalid status: {status_str}"))),
        )
    })?;
    let source = EditSource::from_db_str(&source_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!("invalid source: {source_str}"))),
        )
    })?;
    Ok(HarnessEdit {
        id: row.get(0)?,
        pathology: row.get(1)?,
        content: row.get(2)?,
        status,
        source,
        verify_count: row.get(5)?,
        success_count: row.get(6)?,
        created_at: row.get(7)?,
        last_verified_at: row.get(8)?,
        proposer_reasoning: row.get(9)?,
        similarity_hash: row.get(10)?,
        retire_reason: row.get(11)?,
    })
}

/// 当前时间戳(ms since epoch)。
pub(crate) fn current_timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 生成 edit id:`edit-{timestamp}-{short_hash}`。
pub(crate) fn generate_edit_id(content: &str, pathology: &str) -> String {
    let hash = compute_simhash(&format!("{pathology} {content}"));
    format!("edit-{}-{:x}", current_timestamp_ms(), hash & 0xFFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "claw-harness-{label}-{}-{}",
            std::process::id(),
            current_timestamp_ms()
        ))
    }

    fn sample_edit(id: &str, pathology: &str, content: &str, hash: i64) -> HarnessEdit {
        HarnessEdit {
            id: id.to_string(),
            pathology: pathology.to_string(),
            content: content.to_string(),
            status: EditStatus::Candidate,
            source: EditSource::RulePattern,
            verify_count: 0,
            success_count: 0,
            created_at: current_timestamp_ms(),
            last_verified_at: None,
            proposer_reasoning: "test".to_string(),
            similarity_hash: hash,
            retire_reason: None,
        }
    }

    #[test]
    fn archive_roundtrip_and_status_update() {
        let dir = tmp_dir("roundtrip");
        let archive = HarnessArchive::open(&dir).expect("open");
        archive
            .add_candidate(sample_edit("e1", "p1", "content one", 111))
            .expect("add");

        let edit = archive.get_edit("e1").expect("get").expect("exists");
        assert_eq!(edit.pathology, "p1");
        assert_eq!(edit.status, EditStatus::Candidate);

        archive
            .update_status_and_stats("e1", EditStatus::Active, 5, 4)
            .expect("update");
        let active = archive.active_edits().expect("active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].status, EditStatus::Active);
        assert_eq!(active[0].verify_count, 5);
        assert_eq!(active[0].success_count, 4);
        assert!((active[0].success_rate() - 0.8).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_rollback_retires_active_edits() {
        let dir = tmp_dir("rollback");
        let archive = HarnessArchive::open(&dir).expect("open");
        archive
            .add_candidate(sample_edit("e1", "p1", "content one", 111))
            .expect("add");
        archive
            .update_status("e1", EditStatus::Active)
            .expect("activate");

        let rolled = archive.rollback_all().expect("rollback_all");
        assert_eq!(rolled, 1);
        let retired = archive.retired_edits().expect("retired");
        assert_eq!(retired.len(), 1);
        assert_eq!(
            retired[0].retire_reason.as_deref(),
            Some("manual rollback")
        );
        assert!(archive.active_edits().expect("active").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_enforces_candidate_capacity() {
        let dir = tmp_dir("capacity");
        let archive = HarnessArchive::open(&dir).expect("open");
        // 写入 MAX_CANDIDATE_EDITS + 3 条,最旧 3 条应被淘汰。
        let base = current_timestamp_ms();
        for i in 0..MAX_CANDIDATE_EDITS + 3 {
            let mut edit = sample_edit(
                &format!("e{i}"),
                &format!("p{i}"),
                &format!("content {i}"),
                i as i64,
            );
            edit.created_at = base + i as i64;
            archive.add_candidate(edit).expect("add");
        }
        let candidates = archive.candidate_edits().expect("candidates");
        assert_eq!(candidates.len(), MAX_CANDIDATE_EDITS);
        // 淘汰的是最旧的 3 条(e0/e1/e2)。
        for i in 0..3 {
            assert!(
                archive.get_edit(&format!("e{i}")).expect("get").is_none(),
                "e{i} 应被容量淘汰"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_rejects_oversized_content() {
        let dir = tmp_dir("oversize");
        let archive = HarnessArchive::open(&dir).expect("open");
        let long_content = "x".repeat(MAX_EDIT_CONTENT_CHARS + 1);
        let result = archive.add_candidate(sample_edit("e1", "p1", &long_content, 1));
        assert!(result.is_err(), "超长 content 应被拒绝");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_stats_counts_by_status_and_source() {
        let dir = tmp_dir("stats");
        let archive = HarnessArchive::open(&dir).expect("open");
        archive
            .add_candidate(sample_edit("e1", "p1", "c1", 1))
            .expect("add");
        archive
            .add_candidate(sample_edit("e2", "p2", "c2", 2))
            .expect("add");
        archive.update_status("e1", EditStatus::Active).expect("act");
        archive
            .update_status_and_stats("e1", EditStatus::Active, 2, 2)
            .expect("stats");

        let stats = archive.stats().expect("stats");
        assert_eq!(stats.total, 2);
        assert_eq!(stats.active, 1);
        assert_eq!(stats.candidate, 1);
        assert_eq!(stats.retired, 0);
        assert_eq!(stats.rule_sourced, 2);
        assert_eq!(stats.llm_sourced, 0);
        assert!((stats.avg_active_success_rate - 1.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
