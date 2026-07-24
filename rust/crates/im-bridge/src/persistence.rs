//! Session persistence: save and restore IM bridge sessions to/from disk.
//!
//! Sessions are stored as JSON at `~/.claw/im-bridge-sessions.json`.
//! Auto-save runs periodically and on shutdown.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::session::ChatKey;

/// A serializable session snapshot for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub platform: String,
    pub chat_id: String,
    pub session_id: String,
    pub cwd: String,
    pub last_active_secs: u64,
    /// The user who started this session.
    pub user_id: Option<String>,
}

/// The full persistence file contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceData {
    /// Sessions keyed by "platform:chat_id".
    pub sessions: Vec<PersistedSession>,
    /// Schema version for future compatibility.
    pub schema_version: u32,
}

impl Default for PersistenceData {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            schema_version: 1,
        }
    }
}

/// Manages session persistence to disk.
pub struct PersistenceManager {
    path: PathBuf,
    /// Auto-save interval.
    save_interval: Duration,
}

impl Default for PersistenceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistenceManager {
    /// Create a new persistence manager.
    ///
    /// Stores at `~/.claw/im-bridge-sessions.json`.
    pub fn new() -> Self {
        let path = Self::default_path();
        Self {
            path,
            save_interval: Duration::from_secs(60),
        }
    }

    /// Create with a custom path (for testing).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            save_interval: Duration::from_secs(60),
        }
    }

    /// Set auto-save interval.
    #[allow(dead_code)]
    pub fn with_save_interval(mut self, interval: Duration) -> Self {
        self.save_interval = interval;
        self
    }

    /// Get the auto-save interval.
    pub fn save_interval(&self) -> Duration {
        self.save_interval
    }

    /// Load persisted sessions from disk.
    ///
    /// Returns empty data if the file doesn't exist or is corrupted.
    pub fn load(&self) -> PersistenceData {
        match std::fs::read_to_string(&self.path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!("failed to parse persistence file: {e}, starting fresh");
                PersistenceData::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no existing persistence file, starting fresh");
                PersistenceData::default()
            }
            Err(e) => {
                tracing::warn!("failed to read persistence file: {e}, starting fresh");
                PersistenceData::default()
            }
        }
    }

    /// Save sessions to disk.
    pub fn save(&self, data: &PersistenceData) -> Result<(), String> {
        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create persistence dir: {e}"))?;
        }

        let content =
            serde_json::to_string_pretty(data).map_err(|e| format!("serialize failed: {e}"))?;

        // Atomic write: write to temp file, then rename
        let tmp_path = self.path.with_extension("tmp");
        std::fs::write(&tmp_path, content).map_err(|e| format!("write failed: {e}"))?;
        std::fs::rename(&tmp_path, &self.path).map_err(|e| format!("rename failed: {e}"))?;

        tracing::debug!(
            "persisted {} sessions to {}",
            data.sessions.len(),
            self.path.display()
        );
        Ok(())
    }

    /// Build `PersistedSession` from an active chat session.
    pub fn build_persisted_session(
        key: &ChatKey,
        session_id: &acp::SessionId,
        cwd: &std::path::Path,
        last_active: Instant,
        user_id: Option<&str>,
    ) -> PersistedSession {
        PersistedSession {
            platform: key.platform.clone(),
            chat_id: key.chat_id.clone(),
            session_id: session_id.to_string(),
            cwd: cwd.display().to_string(),
            last_active_secs: last_active.elapsed().as_secs(),
            user_id: user_id.map(str::to_string),
        }
    }

    fn default_path() -> PathBuf {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".claw").join("im-bridge-sessions.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_persistence_roundtrip() {
        let tmp = std::env::temp_dir().join("im-bridge-test-sessions.json");
        let _ = std::fs::remove_file(&tmp);

        let mgr = PersistenceManager::with_path(tmp.clone());

        let data = PersistenceData {
            sessions: vec![PersistedSession {
                platform: "feishu".to_string(),
                chat_id: "chat_123".to_string(),
                session_id: "sess_456".to_string(),
                cwd: "/tmp".to_string(),
                last_active_secs: 60,
                user_id: Some("user_abc".to_string()),
            }],
            schema_version: 1,
        };

        mgr.save(&data).unwrap();

        let loaded = mgr.load();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].platform, "feishu");
        assert_eq!(loaded.sessions[0].chat_id, "chat_123");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_persistence_empty_on_missing_file() {
        let mgr = PersistenceManager::with_path(PathBuf::from("/nonexistent/path/test.json"));
        let data = mgr.load();
        assert!(data.sessions.is_empty());
    }
}
