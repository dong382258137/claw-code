//! Phase 5：每工作区会话读取索引（`<workspace>/.claw/sessions.db`）。
//!
//! JSONL 仍是正文的唯一真相源；本模块只缓存会话列表元数据与每条消息在文件中的
//! 字节区间，供 Web 端快速列会话、按区间读取单条消息。索引可随时删除，下一次
//! 访问会按目录内容自动重建（见 `index_session_file`）。

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS index_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    path TEXT NOT NULL DEFAULT '',
    title TEXT,
    created_at_ms INTEGER NOT NULL DEFAULT 0,
    updated_at_ms INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 0,
    jsonl_size INTEGER NOT NULL DEFAULT 0,
    jsonl_mtime_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS sessions_workspace_updated
    ON sessions(workspace, jsonl_mtime_ms DESC);
CREATE TABLE IF NOT EXISTS messages (
    session_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    byte_start INTEGER NOT NULL,
    byte_end INTEGER NOT NULL,
    role TEXT NOT NULL DEFAULT '',
    timestamp_ms INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, idx)
);
"#;

#[derive(Debug)]
pub enum SessionIndexError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Format(String),
}

impl std::fmt::Display for SessionIndexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "sqlite: {error}"),
            Self::Io(error) => write!(formatter, "io: {error}"),
            Self::Format(message) => write!(formatter, "format: {message}"),
        }
    }
}

impl std::error::Error for SessionIndexError {}

impl From<rusqlite::Error> for SessionIndexError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<std::io::Error> for SessionIndexError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// `sessions` 表的一行：列表渲染所需字段。
#[derive(Debug, Clone, Default)]
pub struct SessionIndexEntry {
    pub id: String,
    pub workspace: String,
    pub path: String,
    pub title: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub message_count: i64,
    pub jsonl_size: i64,
    pub jsonl_mtime_ms: i64,
}

/// 单条消息在 JSONL 中的字节区间（`end` 含行尾换行）。
#[derive(Debug, Clone)]
pub struct MessageSpan {
    pub idx: usize,
    pub byte_start: u64,
    pub byte_end: u64,
    pub role: String,
    pub timestamp_ms: i64,
}

pub struct SessionIndex {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl std::fmt::Debug for SessionIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionIndex")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SessionIndex {
    pub fn open(path: &Path) -> Result<Self, SessionIndexError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // WAL 让 Web 读与 turn 写入可以并发；索引丢一两条也可重建。
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES('schema_version', '1')",
            [],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn upsert_session(&self, entry: &SessionIndexEntry) -> Result<(), SessionIndexError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sessions(id, workspace, path, title, created_at_ms, updated_at_ms, \
             message_count, jsonl_size, jsonl_mtime_ms) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET \
             workspace=excluded.workspace, path=excluded.path, \
             title=COALESCE(excluded.title, sessions.title), \
             created_at_ms=excluded.created_at_ms, updated_at_ms=excluded.updated_at_ms, \
             message_count=excluded.message_count, jsonl_size=excluded.jsonl_size, \
             jsonl_mtime_ms=excluded.jsonl_mtime_ms",
            params![
                entry.id,
                entry.workspace,
                entry.path,
                entry.title,
                entry.created_at_ms,
                entry.updated_at_ms,
                entry.message_count,
                entry.jsonl_size,
                entry.jsonl_mtime_ms,
            ],
        )?;
        Ok(())
    }

    /// 追加一条消息：更新会话行 + 写入消息区间（一个事务）。
    pub fn append_message(
        &self,
        entry: &SessionIndexEntry,
        span: &MessageSpan,
    ) -> Result<(), SessionIndexError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions(id, workspace, path, title, created_at_ms, updated_at_ms, \
             message_count, jsonl_size, jsonl_mtime_ms) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET \
             workspace=excluded.workspace, path=excluded.path, \
             title=COALESCE(excluded.title, sessions.title), \
             created_at_ms=excluded.created_at_ms, updated_at_ms=excluded.updated_at_ms, \
             message_count=excluded.message_count, jsonl_size=excluded.jsonl_size, \
             jsonl_mtime_ms=excluded.jsonl_mtime_ms",
            params![
                entry.id,
                entry.workspace,
                entry.path,
                entry.title,
                entry.created_at_ms,
                entry.updated_at_ms,
                entry.message_count,
                entry.jsonl_size,
                entry.jsonl_mtime_ms,
            ],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO messages(session_id, idx, byte_start, byte_end, role, timestamp_ms) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entry.id,
                i64::try_from(span.idx).unwrap_or(i64::MAX),
                i64::try_from(span.byte_start).unwrap_or(i64::MAX),
                i64::try_from(span.byte_end).unwrap_or(i64::MAX),
                span.role,
                span.timestamp_ms,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 整份会话重写后一次性替换消息区间（turn 结束、压缩等场景）。
    pub fn replace_messages(
        &self,
        entry: &SessionIndexEntry,
        spans: &[MessageSpan],
    ) -> Result<(), SessionIndexError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions(id, workspace, path, title, created_at_ms, updated_at_ms, \
             message_count, jsonl_size, jsonl_mtime_ms) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET \
             workspace=excluded.workspace, path=excluded.path, \
             title=COALESCE(excluded.title, sessions.title), \
             created_at_ms=excluded.created_at_ms, updated_at_ms=excluded.updated_at_ms, \
             message_count=excluded.message_count, jsonl_size=excluded.jsonl_size, \
             jsonl_mtime_ms=excluded.jsonl_mtime_ms",
            params![
                entry.id,
                entry.workspace,
                entry.path,
                entry.title,
                entry.created_at_ms,
                entry.updated_at_ms,
                entry.message_count,
                entry.jsonl_size,
                entry.jsonl_mtime_ms,
            ],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![entry.id],
        )?;
        {
            let mut statement = tx.prepare(
                "INSERT OR REPLACE INTO messages(session_id, idx, byte_start, byte_end, role, timestamp_ms) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for span in spans {
                statement.execute(params![
                    entry.id,
                    i64::try_from(span.idx).unwrap_or(i64::MAX),
                    i64::try_from(span.byte_start).unwrap_or(i64::MAX),
                    i64::try_from(span.byte_end).unwrap_or(i64::MAX),
                    span.role,
                    span.timestamp_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_session(&self, session_id: &str) -> Result<(), SessionIndexError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_sessions(
        &self,
        workspace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<SessionIndexEntry>, usize), SessionIndexError> {
        let conn = self.lock();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE workspace = ?1 AND message_count > 0",
            params![workspace],
            |row| row.get(0),
        )?;
        let mut statement = conn.prepare(
            "SELECT id, workspace, path, title, created_at_ms, updated_at_ms, \
             message_count, jsonl_size, jsonl_mtime_ms \
             FROM sessions WHERE workspace = ?1 AND message_count > 0 \
             ORDER BY jsonl_mtime_ms DESC, updated_at_ms DESC LIMIT ?2 OFFSET ?3",
        )?;
        let entries = statement
            .query_map(
                params![
                    workspace,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    i64::try_from(offset).unwrap_or(i64::MAX),
                ],
                entry_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((entries, usize::try_from(total).unwrap_or(usize::MAX)))
    }

    pub fn get_session(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionIndexEntry>, SessionIndexError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT id, workspace, path, title, created_at_ms, updated_at_ms, \
             message_count, jsonl_size, jsonl_mtime_ms FROM sessions WHERE id = ?1",
        )?;
        let mut rows = statement.query(params![session_id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(entry_from_row(row)?))
    }

    /// 重建时用于比对的文件快照：`(id, path, size, mtime_ms)`。
    pub fn known_files(&self) -> Result<Vec<(String, String, i64, i64)>, SessionIndexError> {
        let conn = self.lock();
        let mut statement =
            conn.prepare("SELECT id, path, jsonl_size, jsonl_mtime_ms FROM sessions")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn message_span(
        &self,
        session_id: &str,
        idx: usize,
    ) -> Result<Option<(u64, u64)>, SessionIndexError> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT byte_start, byte_end FROM messages WHERE session_id = ?1 AND idx = ?2",
        )?;
        let mut rows =
            statement.query(params![session_id, i64::try_from(idx).unwrap_or(i64::MAX)])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let start: i64 = row.get(0)?;
        let end: i64 = row.get(1)?;
        Ok(Some((
            u64::try_from(start).unwrap_or(0),
            u64::try_from(end).unwrap_or(0),
        )))
    }
}

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionIndexEntry> {
    Ok(SessionIndexEntry {
        id: row.get(0)?,
        workspace: row.get(1)?,
        path: row.get(2)?,
        title: row.get(3)?,
        created_at_ms: row.get(4)?,
        updated_at_ms: row.get(5)?,
        message_count: row.get(6)?,
        jsonl_size: row.get(7)?,
        jsonl_mtime_ms: row.get(8)?,
    })
}

/// 从会话文件路径推导工作区根（`<ws>/.claw/sessions/<fingerprint>/<id>.jsonl`）。
///
/// 索引与查询都以它为 `workspace` 键，避免直接使用可能不同大小写/未规范化的
/// `workspace_root` 导致列表查不到。
#[must_use]
pub fn workspace_label_for_session_path(path: &Path) -> Option<String> {
    let fingerprint_dir = path.parent()?;
    let sessions_dir = fingerprint_dir.parent()?;
    let claw_dir = sessions_dir.parent()?;
    if sessions_dir.file_name()? != "sessions" || claw_dir.file_name()? != ".claw" {
        return None;
    }
    Some(claw_dir.parent()?.display().to_string())
}

/// 解析一整份 JSONL，写入会话行与消息区间（旧会话懒建索引 / 重建场景）。
pub fn index_session_file(
    index: &SessionIndex,
    path: &Path,
    workspace: &str,
) -> Result<SessionIndexEntry, SessionIndexError> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let updated_fs_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        });
    let mut reader = BufReader::new(file);
    let mut offset = 0u64;
    let mut line = String::new();
    let mut spans: Vec<MessageSpan> = Vec::new();
    let mut entry = SessionIndexEntry {
        id: path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string(),
        workspace: workspace.to_string(),
        path: path.display().to_string(),
        title: None,
        created_at_ms: 0,
        updated_at_ms: updated_fs_ms,
        message_count: 0,
        jsonl_size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
        jsonl_mtime_ms: updated_fs_ms,
    };
    let mut last_message_ts = 0i64;
    // 旧版 `.json` 会话是单个 JSON 对象（messages 数组在一行内），没有逐消息
    // 字节区间；这里只保留列表所需的计数与标题，block 读取会回退到整份加载。
    let mut legacy_value: Option<serde_json::Value> = None;
    loop {
        line.clear();
        let start = offset;
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        offset += u64::try_from(read).unwrap_or(u64::MAX);
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        match value.get("type").and_then(|kind| kind.as_str()) {
            Some("session_meta") => {
                if let Some(id) = value.get("session_id").and_then(|id| id.as_str()) {
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                entry.created_at_ms = value
                    .get("created_at_ms")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                entry.updated_at_ms = value
                    .get("updated_at_ms")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(entry.updated_at_ms);
            }
            Some("message") => {
                let message = &value["message"];
                let role = message
                    .get("role")
                    .and_then(|role| role.as_str())
                    .unwrap_or("");
                let timestamp_ms = message
                    .get("ts")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                if timestamp_ms > 0 {
                    last_message_ts = timestamp_ms;
                }
                if entry.title.is_none() && role == "user" {
                    entry.title = message
                        .get("blocks")
                        .and_then(|blocks| blocks.as_array())
                        .into_iter()
                        .flatten()
                        .find_map(|block| {
                            if block.get("type").and_then(|kind| kind.as_str()) == Some("text") {
                                block.get("text").and_then(|text| text.as_str())
                            } else {
                                None
                            }
                        })
                        .map(short_title);
                }
                spans.push(MessageSpan {
                    idx: spans.len(),
                    byte_start: start,
                    byte_end: offset,
                    role: role.to_string(),
                    timestamp_ms,
                });
            }
            _ => {
                if value
                    .get("messages")
                    .is_some_and(serde_json::Value::is_array)
                {
                    legacy_value = Some(value);
                }
            }
        }
    }
    if spans.is_empty() {
        if let Some(value) = legacy_value {
            if let Some(id) = value.get("session_id").and_then(|id| id.as_str()) {
                if !id.is_empty() {
                    entry.id = id.to_string();
                }
            }
            if let Some(created) = value
                .get("created_at_ms")
                .and_then(serde_json::Value::as_i64)
            {
                entry.created_at_ms = created;
            }
            if let Some(updated) = value
                .get("updated_at_ms")
                .and_then(serde_json::Value::as_i64)
            {
                if updated > 0 {
                    entry.updated_at_ms = updated;
                }
            }
            if let Some(messages) = value.get("messages").and_then(|value| value.as_array()) {
                entry.message_count = i64::try_from(messages.len()).unwrap_or(i64::MAX);
                entry.title = messages.iter().find_map(|message| {
                    if message.get("role").and_then(|role| role.as_str()) != Some("user") {
                        return None;
                    }
                    message
                        .get("blocks")
                        .and_then(|blocks| blocks.as_array())
                        .into_iter()
                        .flatten()
                        .find_map(|block| {
                            if block.get("type").and_then(|kind| kind.as_str()) == Some("text") {
                                block
                                    .get("text")
                                    .and_then(|text| text.as_str())
                                    .map(short_title)
                            } else {
                                None
                            }
                        })
                });
            }
        }
    }
    entry.message_count = entry
        .message_count
        .max(i64::try_from(spans.len()).unwrap_or(i64::MAX));
    if entry.updated_at_ms <= 0 {
        entry.updated_at_ms = last_message_ts;
    }
    index.replace_messages(&entry, &spans)?;
    Ok(entry)
}

fn short_title(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    let mut title: String = first_line.chars().take(80).collect();
    if title.is_empty() {
        title = "Untitled".to_string();
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(dir: &Path, id: &str) -> PathBuf {
        let path = dir
            .join(".claw")
            .join("sessions")
            .join("fp")
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let mut file = fs::File::create(&path).expect("create");
        writeln!(
            file,
            r#"{{"type":"session_meta","version":2,"session_id":"{id}","created_at_ms":1,"updated_at_ms":2}}"#
        )
        .expect("meta");
        writeln!(
            file,
            r#"{{"type":"message","message":{{"role":"user","blocks":[{{"type":"text","text":"hello world"}}],"ts":10}}}}"#
        )
        .expect("user");
        writeln!(
            file,
            r#"{{"type":"message","message":{{"role":"assistant","blocks":[{{"type":"text","text":"hi"}}],"ts":20}}}}"#
        )
        .expect("assistant");
        path
    }

    #[test]
    fn indexes_file_and_reads_message_spans() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        let path = write_session(temp.path(), "session-1");
        let workspace = temp.path().display().to_string();
        let index =
            SessionIndex::open(&temp.path().join(".claw").join("sessions.db")).expect("open index");

        // when
        let entry = index_session_file(&index, &path, &workspace).expect("index file");

        // then
        assert_eq!(entry.id, "session-1");
        assert_eq!(entry.message_count, 2);
        assert_eq!(entry.title.as_deref(), Some("hello world"));
        let (listed, total) = index
            .list_sessions(&workspace, 0, 10)
            .expect("list sessions");
        assert_eq!(total, 1);
        assert_eq!(listed.len(), 1);

        let bytes = fs::read(&path).expect("read jsonl");
        let (start, end) = index
            .message_span("session-1", 0)
            .expect("span query")
            .expect("span exists");
        let line = std::str::from_utf8(&bytes[start as usize..end as usize]).expect("utf8");
        assert!(line.contains("\"hello world\""));
        assert!(index.message_span("session-1", 9).expect("query").is_none());
    }

    #[test]
    fn header_only_sessions_are_not_listed() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().display().to_string();
        let index =
            SessionIndex::open(&temp.path().join(".claw").join("sessions.db")).expect("open index");
        let entry = SessionIndexEntry {
            id: "session-empty".to_string(),
            workspace: workspace.clone(),
            path: "unused".to_string(),
            message_count: 0,
            ..SessionIndexEntry::default()
        };
        index.upsert_session(&entry).expect("upsert");

        // when
        let (listed, total) = index
            .list_sessions(&workspace, 0, 10)
            .expect("list sessions");

        // then
        assert_eq!(total, 0);
        assert!(listed.is_empty());
    }

    #[test]
    fn append_message_round_trips_and_remove_clears_rows() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().display().to_string();
        let index =
            SessionIndex::open(&temp.path().join(".claw").join("sessions.db")).expect("open index");
        let entry = SessionIndexEntry {
            id: "session-2".to_string(),
            workspace: workspace.clone(),
            path: "unused".to_string(),
            title: Some("greeting".to_string()),
            created_at_ms: 1,
            updated_at_ms: 2,
            message_count: 1,
            jsonl_size: 64,
            jsonl_mtime_ms: 3,
        };
        let span = MessageSpan {
            idx: 0,
            byte_start: 10,
            byte_end: 40,
            role: "user".to_string(),
            timestamp_ms: 5,
        };

        // when
        index.append_message(&entry, &span).expect("append");
        index.remove_session("session-2").expect("remove");

        // then
        assert!(index
            .message_span("session-2", 0)
            .expect("span query")
            .is_none());
        let (listed, total) = index.list_sessions(&workspace, 0, 10).expect("list");
        assert_eq!(total, 0);
        assert!(listed.is_empty());
    }

    #[test]
    fn workspace_label_is_derived_from_session_path() {
        let path = Path::new("D:/repo/.claw/sessions/fp/session-1.jsonl");
        assert_eq!(
            workspace_label_for_session_path(path).as_deref(),
            Some("D:/repo")
        );
        assert!(workspace_label_for_session_path(Path::new("D:/repo/session.jsonl")).is_none());
    }
}
