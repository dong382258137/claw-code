//! JSON persistence layer for the [`PersistentMemory`](crate::memory::PersistentMemory) surface.
//!
//! The store reads and writes a single JSON file under
//! `<workspace>/.claw/memory.json`. It is intentionally minimal: no schema
//! migration, no atomic writes (the [`PersistentMemory`] layer is the source
//! of truth during a session and rewrites the whole file on every save).

use std::fs;
use std::path::{Path, PathBuf};

use crate::memory::PersistentMemory;

/// Filesystem-backed store for [`PersistentMemory`].
///
/// One store corresponds to one JSON file on disk. The store is stateless
/// beyond the file path — every [`MemoryStore::load`] and [`MemoryStore::save`]
/// performs a fresh IO round-trip, which keeps the in-memory model in
/// `memory.rs` as the single source of truth during a session.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    file_path: PathBuf,
}

impl MemoryStore {
    /// Create a new store pointing at `file_path`.
    #[must_use]
    pub fn new(file_path: PathBuf) -> Self {
        Self { file_path }
    }

    /// Resolve the canonical memory file path for a workspace root.
    ///
    /// Returns `<workspace>/.claw/memory.json`. The directory may not exist
    /// yet; [`MemoryStore::save`] creates it on demand.
    #[must_use]
    pub fn resolve_for_workspace(workspace: &Path) -> PathBuf {
        workspace.join(".claw").join("memory.json")
    }

    /// Borrow the on-disk file path this store points at.
    #[must_use]
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Load the memory file from disk.
    ///
    /// Returns:
    /// * `Ok(Some(memory))` when the file exists and parses cleanly.
    /// * `Ok(None)` when the file does not exist or is empty. Callers
    ///   should fall back to an empty memory surface.
    /// * `Err(_)` on read or parse failures — the caller decides whether to
    ///   surface the error or treat it as a missing memory.
    pub fn load(&self) -> std::io::Result<Option<PersistentMemory>> {
        match fs::read_to_string(&self.file_path) {
            Ok(content) if content.trim().is_empty() => Ok(None),
            Ok(content) => {
                let memory: PersistentMemory = serde_json::from_str(&content)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
                Ok(Some(memory))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Persist `memory` to disk, creating parent directories as needed.
    ///
    /// The full file is rewritten on every save — there is no incremental
    /// update. This keeps the on-disk format simple and matches the
    /// session-snapshot model: the in-memory state is authoritative during
    /// a turn, and the file is just a recovery checkpoint.
    pub fn save(&self, memory: &PersistentMemory) -> std::io::Result<()> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(memory)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        let tmp = self.file_path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &self.file_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryBlock, MemoryEntry, PersistentMemory};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("claw-memory-store-{nanos}.json"))
    }

    #[test]
    fn resolve_for_workspace_uses_claw_subdirectory() {
        let path = MemoryStore::resolve_for_workspace(Path::new("/tmp/project"));
        assert!(path.ends_with(".claw/memory.json"));
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let store = MemoryStore::new(temp_path());
        let loaded = store.load().expect("missing file should not error");
        assert!(loaded.is_none());
    }

    #[test]
    fn load_returns_none_when_file_empty() {
        let path = temp_path();
        std::fs::write(&path, "   \n  ").expect("write empty file");
        let store = MemoryStore::new(path.clone());
        let loaded = store.load().expect("empty file should not error");
        assert!(loaded.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_then_load_round_trips_entries_and_blocks() {
        let path = temp_path();
        let store = MemoryStore::new(path.clone());

        let mut mem = PersistentMemory::empty(path.clone());
        mem.blocks_mut()[0].replace_content("assistant persona".to_string());
        mem.blocks_mut()[1].replace_content("user facts".to_string());
        mem.add_entry("user prefers dark mode", "session-1");

        store.save(&mem).expect("save should succeed");

        let reloaded = store
            .load()
            .expect("load should succeed")
            .expect("memory should be present");
        assert_eq!(reloaded.blocks()[0].content(), "assistant persona");
        assert_eq!(reloaded.blocks()[1].content(), "user facts");
        assert!(reloaded
            .entries()
            .iter()
            .any(|entry| entry.content == "user prefers dark mode"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_creates_parent_directory_if_missing() {
        let path = temp_path();
        let nested = path.parent().unwrap().join("nested-dir").join("mem.json");
        let store = MemoryStore::new(nested.clone());
        let mem = PersistentMemory::empty(nested.clone());
        store.save(&mem).expect("save should create parents");
        assert!(nested.exists());
        let _ = std::fs::remove_dir_all(nested.parent().unwrap());
    }

    #[test]
    fn load_returns_err_on_corrupted_json() {
        let path = temp_path();
        std::fs::write(&path, "{ not valid json").expect("write corrupted file");
        let store = MemoryStore::new(path.clone());
        let result = store.load();
        assert!(result.is_err(), "corrupted JSON should surface as error");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_blocks_are_persona_human_tasks() {
        let mem = PersistentMemory::empty(temp_path());
        assert_eq!(mem.blocks()[0], MemoryBlock::persona());
        assert_eq!(mem.blocks()[1], MemoryBlock::human());
        assert_eq!(mem.blocks()[2], MemoryBlock::tasks());
    }

    #[test]
    fn memory_entry_supersede_sets_valid_until_and_superseded_by() {
        let mut entry = MemoryEntry::new("old fact", "s1", 1_000);
        entry.supersede("new fact", 2_000);
        assert_eq!(entry.superseded_by.as_deref(), Some("new fact"));
        assert_eq!(entry.valid_until, Some(2_000));
    }
}
