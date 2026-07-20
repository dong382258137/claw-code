//! Long-lived memory layer for `claw`.
//!
//! This module owns the persistent memory surface that survives across
//! sessions. It is heavily inspired by Letta's memory block model and Zep's
//! dual-time validity model, but kept intentionally small: rule-based
//! extraction only, no LLM calls.
//!
//! The layer is split into two concerns:
//!
//! * [`memory_store`] handles JSON persistence under
//!   `<workspace>/.claw/memory.json`.
//! * [`PersistentMemory`] (this module) holds the in-memory domain model
//!   and exposes the mid-session freeze that keeps the prompt-cache prefix
//!   stable across turns within a single session.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::memory_semantic::SemanticRecaller;
use crate::memory_store::MemoryStore;
use crate::session::{ContentBlock, ConversationMessage, MessageRole};

/// Seven days expressed in milliseconds — entries whose `last_verified_at` is
/// older than this threshold are rendered with an `[unverified]` marker.
pub const UNVERIFIED_THRESHOLD_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Fraction of block capacity at which consolidation kicks in.
pub const CONSOLIDATION_CAPACITY_RATIO: f64 = 0.8;

/// Return the current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Derive a stable L1 index id from entry content. Same content → same id,
/// so duplicate entries collapse to a single L1 slot during dedup. Uses
/// the std default hasher (no extra dependency) — collision resistance
/// is sufficient for an in-memory index that is rebuilt on every load.
fn entry_id(content: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("entry-{:016x}", hasher.finish())
}

/// One of the three typed memory blocks mirroring Letta's core memory model.
///
/// Each variant carries its own `max_chars` budget so the renderer can keep
/// the Persona / Human / Tasks prefix stable independently — overflowing a
/// single block only compresses that block, never the others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MemoryBlock {
    Persona {
        content: String,
        max_chars: usize,
    },
    Human {
        content: String,
        max_chars: usize,
    },
    Tasks {
        content: String,
        max_chars: usize,
    },
}

impl MemoryBlock {
    /// Default Persona block with a 500-char budget.
    #[must_use]
    pub fn persona() -> Self {
        Self::Persona {
            content: String::new(),
            max_chars: 500,
        }
    }

    /// Default Human block with a 1000-char budget.
    #[must_use]
    pub fn human() -> Self {
        Self::Human {
            content: String::new(),
            max_chars: 1000,
        }
    }

    /// Default Tasks block with an 800-char budget.
    #[must_use]
    pub fn tasks() -> Self {
        Self::Tasks {
            content: String::new(),
            max_chars: 800,
        }
    }

    /// Borrow the block's textual content.
    #[must_use]
    pub fn content(&self) -> &str {
        match self {
            Self::Persona { content, .. }
            | Self::Human { content, .. }
            | Self::Tasks { content, .. } => content,
        }
    }

    /// Borrow the block's mutable textual content.
    pub fn content_mut(&mut self) -> &mut String {
        match self {
            Self::Persona { content, .. }
            | Self::Human { content, .. }
            | Self::Tasks { content, .. } => content,
        }
    }

    /// The character budget for this block.
    #[must_use]
    pub fn max_chars(&self) -> usize {
        match self {
            Self::Persona { max_chars, .. }
            | Self::Human { max_chars, .. }
            | Self::Tasks { max_chars, .. } => *max_chars,
        }
    }

    /// Replace the textual content in place.
    pub fn replace_content(&mut self, new_content: String) {
        match self {
            Self::Persona { content, .. }
            | Self::Human { content, .. }
            | Self::Tasks { content, .. } => *content = new_content,
        }
    }

    /// Stable human-readable label used when rendering the block.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Persona { .. } => "Persona",
            Self::Human { .. } => "Human",
            Self::Tasks { .. } => "Tasks",
        }
    }

    /// Whether the current content exceeds the configured `max_chars` budget.
    #[must_use]
    pub fn is_over_capacity(&self) -> bool {
        self.content().chars().count() > self.max_chars()
    }

    /// Fraction of the budget currently in use (`0.0` .. `1.0+`).
    #[must_use]
    pub fn capacity_ratio(&self) -> f64 {
        let max = self.max_chars().max(1);
        let cur = self.content().chars().count();
        cur as f64 / max as f64
    }
}

/// Default constructor used by serde when a block is missing on disk.
fn default_blocks() -> [MemoryBlock; 3] {
    [MemoryBlock::persona(), MemoryBlock::human(), MemoryBlock::tasks()]
}

/// One memory fact with a temporal validity envelope.
///
/// Modelled after Zep's dual-time graph: every fact has `valid_from` and an
/// optional `valid_until`. When a newer fact supersedes an older one, the
/// older entry's `valid_until` and `superseded_by` are populated rather than
/// deleting it — this preserves audit history and lets the renderer reach
/// back into superseded facts if needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub content: String,
    /// Unix timestamp (ms) when this entry became effective.
    pub valid_from: i64,
    /// Optional Unix timestamp (ms) marking the entry as no longer current.
    pub valid_until: Option<i64>,
    /// Content (or pattern) of the entry that superseded this one, if any.
    pub superseded_by: Option<String>,
    /// Provenance tag, e.g. `"session-2026-01-15#msg42"`.
    pub source: String,
    /// Unix timestamp (ms) of the last time this fact was independently
    /// verified. Entries older than [`UNVERIFIED_THRESHOLD_MS`] are rendered
    /// with an `[unverified]` marker.
    pub last_verified_at: i64,
}

impl MemoryEntry {
    /// Create a new active entry with `valid_from` set to `now_ms`.
    #[must_use]
    pub fn new(content: impl Into<String>, source: impl Into<String>, now_ms: i64) -> Self {
        Self {
            content: content.into(),
            valid_from: now_ms,
            valid_until: None,
            superseded_by: None,
            source: source.into(),
            last_verified_at: now_ms,
        }
    }

    /// Whether this entry is still active at the given time.
    ///
    /// Active means: not yet expired and not yet superseded.
    #[must_use]
    pub fn is_active(&self, now_ms: i64) -> bool {
        let not_expired = match self.valid_until {
            None => true,
            Some(until) => now_ms < until,
        };
        not_expired && self.superseded_by.is_none()
    }

    /// Whether this entry's `last_verified_at` is older than 7 days.
    #[must_use]
    pub fn is_unverified(&self, now_ms: i64) -> bool {
        now_ms - self.last_verified_at > UNVERIFIED_THRESHOLD_MS
    }

    /// Mark this entry as superseded by a newer entry.
    pub fn supersede(&mut self, by_content: impl Into<String>, now_ms: i64) {
        let by = by_content.into();
        self.superseded_by = Some(by);
        if self.valid_until.is_none() {
            self.valid_until = Some(now_ms);
        }
    }
}

/// Long-lived memory surface backed by a JSON file on disk.
///
/// Holds three typed blocks (Persona / Human / Tasks) for the stable core
/// memory, plus a list of time-stamped [`MemoryEntry`] facts that can be
/// superseded or expired without being deleted.
///
/// The `frozen_snapshot` field is captured at session start by
/// [`PersistentMemory::load_and_freeze`] and is the only view of memory that
/// ever lands in the system prompt within that session — this keeps the
/// prompt-cache prefix stable. New facts written mid-session land on disk
/// immediately but only surface in the next session.
///
/// `entries` holds only currently-active facts. Superseded / expired
/// entries are migrated to `archive` during [`PersistentMemory::consolidate`]
/// so audit history is preserved without bloating the active view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentMemory {
    #[serde(default = "default_blocks")]
    blocks: [MemoryBlock; 3],
    #[serde(default)]
    entries: Vec<MemoryEntry>,
    /// Archived (superseded / expired) entries kept for audit history.
    /// Excluded from rendering. Capped at [`ARCHIVE_MAX_ENTRIES`] during
    /// consolidation; oldest entries are dropped first.
    #[serde(default)]
    archive: Vec<MemoryEntry>,
    /// Path of the on-disk JSON file. Not serialized.
    #[serde(skip)]
    file_path: PathBuf,
    /// Snapshot captured at session start. Not serialized.
    #[serde(skip)]
    frozen_snapshot: Option<String>,
    /// Semantic recall layer (L1 index). Not serialized — rebuilt at
    /// load time from the entries list. Lives on the memory surface so
    /// the runtime can issue [`PersistentMemory::semantic_recall`] without
    /// juggling a second handle.
    #[serde(skip)]
    semantic: SemanticRecaller,
}

/// Maximum number of archived (superseded / expired) entries to retain
/// on disk. Older entries are pruned during consolidation to bound growth.
pub const ARCHIVE_MAX_ENTRIES: usize = 200;

impl PersistentMemory {
    /// Build an empty memory surface pointing at the given file path.
    ///
    /// Captures a `frozen_snapshot` immediately so subsequent calls to
    /// [`PersistentMemory::frozen_render`] return a byte-stable view even
    /// if the caller mutates `entries` before the next session. Previously
    /// `frozen_snapshot` was `None` here and `frozen_render` fell back to
    /// `render_current()` — which called `now_ms()` and could return a
    /// different active-entries set on each call, silently breaking the
    /// prompt-cache prefix stability guarantee (B4).
    #[must_use]
    pub fn empty(file_path: impl Into<PathBuf>) -> Self {
        let mut memory = Self {
            blocks: default_blocks(),
            entries: Vec::new(),
            archive: Vec::new(),
            file_path: file_path.into(),
            frozen_snapshot: None,
            semantic: SemanticRecaller::new(),
        };
        memory.frozen_snapshot = Some(memory.render_current());
        memory
    }

    /// Load the memory file from disk and freeze a snapshot for the session.
    ///
    /// If the file does not exist or is empty, an empty memory is created.
    /// The snapshot is captured immediately after load so the prompt-cache
    /// prefix remains stable for the lifetime of the session, regardless of
    /// any in-memory mutations.
    ///
    /// Also rebuilds the in-memory semantic L1 index from the loaded
    /// entries list (the L1 index is `#[serde(skip)]` because it is a
    /// pure derivative of `entries` and we do not want to persist a
    /// second copy that could drift out of sync).
    pub fn load_and_freeze(file_path: &Path) -> Self {
        let mut memory = match MemoryStore::new(file_path.to_path_buf()).load() {
            Ok(Some(loaded)) => loaded.with_file_path(file_path.to_path_buf()),
            Ok(None) => Self::empty(file_path),
            // Defensive: corrupted file should not nuke the session. Log a
            // fresh memory and proceed — the next successful save will
            // overwrite the bad file.
            Err(_) => Self::empty(file_path),
        };
        // Rebuild semantic L1 index from entries. Only active entries are
        // indexed — superseded / expired ones are in `archive` and not
        // useful for recall.
        let now = now_ms();
        let mut recaller = SemanticRecaller::new();
        for entry in &memory.entries {
            if entry.is_active(now) {
                recaller.add_l1_entry(&entry_id(&entry.content), &entry.content, &entry.source);
            }
        }
        memory.semantic = recaller;
        memory.frozen_snapshot = Some(memory.render_current());
        memory
    }

    /// Set the on-disk file path (used when restoring after load).
    fn with_file_path(mut self, path: PathBuf) -> Self {
        self.file_path = path;
        self
    }

    /// Borrow the three typed blocks.
    #[must_use]
    pub fn blocks(&self) -> &[MemoryBlock; 3] {
        &self.blocks
    }

    /// Mutably borrow the three typed blocks.
    pub fn blocks_mut(&mut self) -> &mut [MemoryBlock; 3] {
        &mut self.blocks
    }

    /// Borrow all stored entries (active only; superseded / expired
    /// are migrated to [`PersistentMemory::archive`] during consolidation).
    #[must_use]
    pub fn entries(&self) -> &[MemoryEntry] {
        &self.entries
    }

    /// Borrow the archived (superseded / expired) entries kept for audit.
    ///
    /// Archive entries are never rendered into the system prompt; they
    /// exist only so callers can reach back into superseded facts when
    /// debugging or auditing memory state.
    #[must_use]
    pub fn archive(&self) -> &[MemoryEntry] {
        &self.archive
    }

    /// Borrow the on-disk file path.
    #[must_use]
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Return the snapshot captured at session start.
    ///
    /// This is the only view of memory that should be injected into the
    /// system prompt: it stays stable across turns within a session even
    /// as new entries are written to disk.
    #[must_use]
    pub fn frozen_render(&self) -> String {
        match &self.frozen_snapshot {
            Some(snapshot) => snapshot.clone(),
            None => self.render_current(),
        }
    }

    /// Render the current in-memory state to a string.
    ///
    /// Used to capture the frozen snapshot and as a fallback when no
    /// snapshot has been captured. Order is fixed: Persona → Human →
    /// Tasks → active entries, so the prefix stays byte-identical for the
    /// same input.
    fn render_current(&self) -> String {
        let mut sections = Vec::new();
        sections.push("# Persistent Memory".to_string());
        sections.push(self.render_blocks());
        let active: Vec<&MemoryEntry> = self.active_entries(now_ms());
        if !active.is_empty() {
            sections.push(render_entries(&active, now_ms()));
        }
        sections.join("\n\n")
    }

    /// Render the three typed blocks in fixed order (Persona → Human → Tasks).
    ///
    /// Empty blocks are still emitted with their label so the prefix length
    /// stays stable across runs even when a block is later filled in — this
    /// matters for prompt cache hit rates.
    #[must_use]
    pub fn render_blocks(&self) -> String {
        let mut sections = Vec::new();
        for block in &self.blocks {
            let label = block.label();
            let content = block.content();
            if content.is_empty() {
                sections.push(format!("## {label}\n_(empty)_"));
            } else {
                sections.push(format!("## {label}\n{content}"));
            }
        }
        sections.join("\n\n")
    }

    /// Append a new entry to the in-memory store and persist to disk.
    ///
    /// Also mirrors the new entry into the semantic L1 index so subsequent
    /// [`PersistentMemory::semantic_recall`] calls can surface it. The
    /// frozen snapshot is NOT mutated — the snapshot is the only view
    /// surfaced to the system prompt within the current session, so new
    /// entries only appear in the next session.
    pub fn add_entry(&mut self, content: &str, source: &str) {
        let now = now_ms();
        self.entries
            .push(MemoryEntry::new(content.to_string(), source.to_string(), now));
        self.semantic.add_l1_entry(&entry_id(content), content, source);
        self.persist_or_warn("add_entry");
    }

    /// Replace the first entry whose content matches `old_content_pattern`
    /// with `new_content`, marking the old entry as superseded.
    ///
    /// If no match is found, the new content is still appended as a fresh
    /// entry — replacement is a no-op on the entries list, but the new fact
    /// still lands on disk. The new content is also mirrored into the
    /// semantic L1 index for [`PersistentMemory::semantic_recall`].
    pub fn replace_entry(&mut self, old_content_pattern: &str, new_content: &str, source: &str) {
        let now = now_ms();
        for entry in &mut self.entries {
            if entry.is_active(now) && entry.content.contains(old_content_pattern) {
                entry.supersede(new_content.to_string(), now);
                break;
            }
        }
        self.entries
            .push(MemoryEntry::new(new_content.to_string(), source.to_string(), now));
        self.semantic.add_l1_entry(&entry_id(new_content), new_content, source);
        self.persist_or_warn("replace_entry");
    }

    /// Semantic recall — return the top-k entries whose L1 summary best
    /// matches `query`. Default strategy is keyword fallback (no embedding
    /// API required). Hits are intended to be appended to the prompt's
    /// dynamic region (after the cacheable prefix), not injected into the
    /// frozen snapshot — see [`PersistentMemory::frozen_render`].
    #[must_use]
    pub fn semantic_recall(&self, query: &str, k: usize) -> Vec<crate::memory_semantic::MemoryHit> {
        self.semantic.semantic_recall(query, k)
    }

    /// Borrow the underlying semantic recaller. Exposed so callers can
    /// persist the L1 index via [`SemanticRecaller::persist_l1_index`]
    /// or reload it via [`SemanticRecaller::load_l1_index`].
    #[must_use]
    pub fn semantic(&self) -> &SemanticRecaller {
        &self.semantic
    }

    /// Mutably borrow the underlying semantic recaller.
    pub fn semantic_mut(&mut self) -> &mut SemanticRecaller {
        &mut self.semantic
    }

    /// Whether any block has crossed the 80% capacity threshold and needs
    /// consolidation. Also triggers when the entry count grows past a
    /// reasonable ceiling so superseded / expired entries get pruned.
    #[must_use]
    pub fn needs_consolidation(&self) -> bool {
        for block in &self.blocks {
            if block.capacity_ratio() >= CONSOLIDATION_CAPACITY_RATIO {
                return true;
            }
        }
        // Prune once we have a meaningful backlog of historical entries.
        self.entries.len() > 50
    }

    /// Consolidate the memory surface.
    ///
    /// Migrates superseded / expired entries from `entries` into `archive`
    /// (audit history), deduplicates active entries with identical content
    /// (keeping the newest), deduplicates archive entries the same way, caps
    /// the archive size at [`ARCHIVE_MAX_ENTRIES`], and compresses any block
    /// that has crossed its `max_chars` budget. The frozen snapshot is NOT
    /// touched — consolidation only affects future sessions.
    ///
    /// Unlike the previous implementation, superseded / expired entries are
    /// **retained** on disk (in `archive`) rather than dropped, preserving
    /// the audit trail promised by the module docs.
    pub fn consolidate(&mut self) {
        let now = now_ms();

        // 1. Partition: active stays in `entries`, others move to `archive`.
        let mut still_active = Vec::with_capacity(self.entries.len());
        for entry in std::mem::take(&mut self.entries) {
            if entry.is_active(now) {
                still_active.push(entry);
            } else {
                self.archive.push(entry);
            }
        }
        self.entries = still_active;

        // 2. Deduplicate active entries by content, keeping the latest
        //    occurrence (entries are pushed in chronological order, so
        //    reverse iteration picks the newest first).
        self.entries = dedup_latest(self.entries.split_off(0));

        // 3. Deduplicate archive the same way, then cap to ARCHIVE_MAX_ENTRIES.
        self.archive = dedup_latest(self.archive.split_off(0));
        if self.archive.len() > ARCHIVE_MAX_ENTRIES {
            // Oldest first (chronological append order), drop from the front.
            let drop_count = self.archive.len() - ARCHIVE_MAX_ENTRIES;
            self.archive.drain(0..drop_count);
        }

        // 4. Compress any block that has crossed its budget.
        for block in &mut self.blocks {
            if block.is_over_capacity() {
                compress_block_content(block);
            }
        }

        self.persist_or_warn("consolidate");
    }

    /// Persist the current in-memory state to disk.
    fn persist(&self) -> std::io::Result<()> {
        MemoryStore::new(self.file_path.clone()).save(self)
    }

    /// Persist and surface failures via stderr instead of silently
    /// dropping them. Memory writes happen mid-turn and cannot bubble
    /// out of the nudge / replace_entry paths without restructuring,
    /// but a warning is enough to flag disk-full / permission issues
    /// without crashing the session.
    fn persist_or_warn(&self, context: &str) {
        if let Err(err) = self.persist() {
            eprintln!(
                "[memory] warning: persist failed ({context}): {err}; path={}",
                self.file_path.display()
            );
        }
    }

    /// Return only entries that are still active at `now_ms`.
    ///
    /// Active means: not yet expired and not yet superseded. Superseded and
    /// expired entries are retained on disk for audit purposes but excluded
    /// from rendering.
    #[must_use]
    pub fn active_entries(&self, now_ms: i64) -> Vec<&MemoryEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.is_active(now_ms))
            .collect()
    }

    /// Detect which existing entries conflict with a proposed new entry.
    ///
    /// Delegates to the free function [`detect_conflicts`]; kept as a method
    /// for ergonomic call sites that already hold a `&PersistentMemory`.
    #[must_use]
    pub fn detect_conflicts(&self, new_entry: &MemoryEntry) -> Vec<usize> {
        let now = now_ms();
        let mut conflicts = Vec::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            if !entry.is_active(now) {
                continue;
            }
            if has_contradictory_phrase(&entry.content, &new_entry.content) {
                conflicts.push(idx);
            }
        }
        conflicts
    }

    /// Mark the entry at `index` as superseded by `new_content`.
    ///
    /// Does not insert the new content; callers are expected to follow up
    /// with [`PersistentMemory::add_entry`] for the replacement fact.
    pub fn supersede(&mut self, index: usize, new_content: &str) {
        let now = now_ms();
        if let Some(entry) = self.entries.get_mut(index) {
            entry.supersede(new_content.to_string(), now);
            self.persist_or_warn("supersede");
        }
    }

    /// Remove the first active entry whose content contains `pattern`,
    /// migrating it directly to `archive` (audit history). Unlike
    /// [`PersistentMemory::replace_entry`], which leaves the superseded
    /// entry in `entries` until the next [`PersistentMemory::consolidate`]
    /// pass, `remove_entry` immediately moves the retired entry into
    /// `archive` so callers can observe the retirement without waiting
    /// for a consolidation trigger.
    ///
    /// Returns `true` if an entry was retired, `false` if no active entry
    /// matched `pattern`.
    pub fn remove_entry(&mut self, pattern: &str) -> bool {
        let now = now_ms();
        let mut found_idx: Option<usize> = None;
        for (idx, entry) in self.entries.iter().enumerate() {
            if entry.is_active(now) && entry.content.contains(pattern) {
                found_idx = Some(idx);
                break;
            }
        }
        if let Some(idx) = found_idx {
            let mut entry = self.entries.remove(idx);
            // Use the pattern itself as `superseded_by` so the archive
            // entry's audit trail records why it was retired.
            entry.supersede(pattern.to_string(), now);
            self.archive.push(entry);
            self.persist_or_warn("remove_entry");
            return true;
        }
        false
    }
}

/// Deduplicate a list of [`MemoryEntry`] by content, keeping the latest
/// occurrence of each duplicate. Entries are assumed to be in chronological
/// (append) order, so reverse iteration picks the newest version first.
fn dedup_latest(entries: Vec<MemoryEntry>) -> Vec<MemoryEntry> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut deduped: Vec<MemoryEntry> = Vec::with_capacity(entries.len());
    for entry in entries.into_iter().rev() {
        if seen.insert(entry.content.clone()) {
            deduped.push(entry);
        }
    }
    deduped.reverse();
    deduped
}

/// Detect entries that conflict with a proposed new entry.
///
/// Heuristic: if both entries contain a memory keyword (`prefer`, `always`,
/// `never`, `use`, `like`, `hate`) — including common conjugations — with
/// the same keyword but different values, they contradict each other.
///
/// Returns the indices of conflicting entries in `existing`, in ascending
/// order. Superseded and expired entries are skipped.
#[must_use]
pub fn detect_conflicts(new_entry: &MemoryEntry, existing: &[MemoryEntry]) -> Vec<usize> {
    let now = now_ms();
    let mut conflicts = Vec::new();
    for (idx, entry) in existing.iter().enumerate() {
        if !entry.is_active(now) {
            continue;
        }
        if has_contradictory_phrase(&entry.content, &new_entry.content) {
            conflicts.push(idx);
        }
    }
    conflicts
}

/// Return a compiled regex for detecting English memory keywords with
/// optional conjugation suffixes. Compiled once and cached for the process
/// lifetime.
fn contradiction_regex_en() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(prefer|always|never|use|like|hate)(?:s|ed|d)?\b\s+(.+?)(?:[.,;\n]|$)",
        )
        .expect("contradiction regex en should compile")
    })
}

/// Return a compiled regex for detecting Chinese memory keywords. Compiled
/// once and cached for the process lifetime.
///
/// Chinese keywords are matched without word boundaries (since `\b` in
/// the `regex` crate is ASCII-only and does not fire between adjacent
/// Han characters). Values are bounded by sentence-ending punctuation
/// (。、!?), commas, semicolons, or newlines — or end of string.
fn contradiction_regex_zh() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Pairs mirror the English keyword set:
        //   偏好 ≈ prefer, 总是 ≈ always, 从不 ≈ never,
        //   使用 ≈ use, 喜欢 ≈ like, 不喜欢/讨厌 ≈ hate.
        Regex::new(r"(偏好|总是|从不|使用|喜欢|不喜欢|讨厌)([^。、!?\n.,;]+)")
            .expect("contradiction regex zh should compile")
    })
}

/// Extract the (keyword, value) pair from a memory assertion.
///
/// English examples:
/// * `"user prefers dark mode"` → `("prefer", "dark mode")`.
///
/// Chinese examples (B7 fix — previously Chinese memory assertions
/// silently bypassed `detect_conflicts`, so contradicting preferences
/// like "偏好深色模式" vs "偏好浅色模式" could coexist on disk):
/// * `"用户偏好深色模式"` → `("偏好", "深色模式")`.
/// * `"总是使用 tabs"` → `("总是", "使用 tabs")`.
///
/// Returns `None` when no memory keyword is found or the captured value is
/// empty.
fn extract_keyword_value(text: &str) -> Option<(String, String)> {
    // Try English regex first (case-insensitive, with word boundary).
    if let Some((kw, val)) = extract_keyword_value_en(text) {
        return Some((kw, val));
    }
    // Fall back to Chinese regex.
    extract_keyword_value_zh(text)
}

fn extract_keyword_value_en(text: &str) -> Option<(String, String)> {
    let re = contradiction_regex_en();
    let caps = re.captures(text)?;
    let kw = caps.get(1)?.as_str().to_lowercase();
    let val = caps.get(2)?.as_str().trim().to_lowercase();
    if val.is_empty() {
        None
    } else {
        Some((kw, val))
    }
}

fn extract_keyword_value_zh(text: &str) -> Option<(String, String)> {
    let re = contradiction_regex_zh();
    let caps = re.captures(text)?;
    let kw = caps.get(1)?.as_str().to_string();
    let val = caps.get(2)?.as_str().trim().to_string();
    if val.is_empty() {
        None
    } else {
        Some((kw, val))
    }
}

/// Whether two sentences contain contradictory memory assertions.
fn has_contradictory_phrase(a: &str, b: &str) -> bool {
    match (extract_keyword_value(a), extract_keyword_value(b)) {
        (Some((ka, va)), Some((kb, vb))) => ka == kb && va != vb,
        _ => false,
    }
}

/// Render a slice of entries as a stable, ordered list.
///
/// Entries that have not been verified in the last 7 days are prefixed with
/// an `[unverified]` marker so the model knows to double-check before
/// relying on them.
#[must_use]
fn render_entries(entries: &[&MemoryEntry], now_ms: i64) -> String {
    let mut lines = vec!["## Active facts".to_string()];
    for entry in entries {
        let marker = if entry.is_unverified(now_ms) {
            "[unverified] "
        } else {
            ""
        };
        lines.push(format!("- {marker}{}", entry.content));
    }
    lines.join("\n")
}

/// Compress a block's content in place to fit within its `max_chars` budget.
///
/// Strategy: keep the first 80% of the budget verbatim, then append a
/// truncation marker so the model knows content was cut. This is a
/// rule-based placeholder; a future iteration can call out to a summariser.
fn compress_block_content(block: &mut MemoryBlock) {
    let max = block.max_chars();
    let content = block.content();
    if content.chars().count() <= max {
        return;
    }
    let keep = max.saturating_sub(20).max(max * 4 / 5);
    let truncated: String = content.chars().take(keep).collect();
    let compressed = format!("{truncated}\n… [memory block compressed]");
    block.replace_content(compressed);
}

/// Configuration for periodic memory curation (P3-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NudgeConfig {
    /// Number of turns between nudge evaluations.
    pub interval_turns: usize,
    /// How many recent turns to scan for candidate facts.
    pub lookback_turns: usize,
    /// Maximum number of actions returned per nudge.
    pub max_entries_per_nudge: usize,
}

impl Default for NudgeConfig {
    fn default() -> Self {
        Self {
            interval_turns: 5,
            lookback_turns: 3,
            max_entries_per_nudge: 3,
        }
    }
}

/// One curation action proposed by the rule-based nudge extractor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NudgeAction {
    /// Append a brand-new fact.
    Add {
        content: String,
        source: String,
    },
    /// Replace an existing fact matching `old_pattern` with `new_content`.
    Replace {
        old_pattern: String,
        new_content: String,
        source: String,
    },
    /// Remove any fact whose content matches `pattern`.
    Remove {
        pattern: String,
        /// Provenance tag for telemetry / debugging.
        source: String,
    },
}

/// Decide whether a nudge should fire this turn.
#[must_use]
pub fn should_nudge(turns_since_last_nudge: usize, config: &NudgeConfig) -> bool {
    turns_since_last_nudge >= config.interval_turns
}

/// Trigger phrases that signal a correction to a previously held fact.
const CORRECTION_PHRASES: &[&str] = &[
    "no, i meant",
    "actually,",
    "correction:",
    "don't do that",
    "i meant",
    "不对",
    "我意思是",
    "更正",
];

/// Trigger keywords that signal an explicit memory request.
const REMEMBER_KEYWORDS: &[&str] = &[
    "remember",
    "记住",
    "prefer",
    "always",
    "never",
    "不要",
    "总是",
    "永恒",
];

/// Trigger keywords that signal a request to forget / retire a fact.
/// Matches produce [`NudgeAction::Remove`]. Like corrections, the actual
/// retired entry is resolved via content match — the keyword itself is
/// not used as the pattern.
const FORGET_KEYWORDS: &[&str] = &["forget", "忘记", "别再记住", "stop remembering"];

/// Extract candidate curation actions from the most recent conversation turns.
///
/// Rule-based only — no LLM fork. Three passes (highest priority first):
/// 1. Forget keywords (`forget`, `忘记`, …). For each, the post-keyword
///    text is used as a content pattern to retire a matching active entry.
///    Emits [`NudgeAction::Remove`].
/// 2. Correction phrases (`no, I meant`, `actually`, …). For each,
///    build a candidate [`MemoryEntry`] from the post-phrase text and call
///    [`PersistentMemory::detect_conflicts`] to find the actual prior entry
///    that contradicts the new statement. If a conflict is found, emit a
///    [`NudgeAction::Replace`] whose `old_pattern` is the real old entry's
///    content — so [`PersistentMemory::replace_entry`] can supersede it.
///    If no conflict is found (e.g. correction without a prior fact), fall
///    back to [`NudgeAction::Add`].
/// 3. Explicit memory keywords (`remember`, `prefer`, `always`,
///    `never`, …). These produce [`NudgeAction::Add`] suggestions. Before
///    adding, we also check `detect_conflicts` so a contradicting keyword
///    (e.g. `prefer light mode` while an existing entry says `prefer dark
///    mode`) supersedes the old fact instead of producing two active
///    contradictions on disk.
///
/// At most `config.max_entries_per_nudge` actions are returned, prioritising
/// forgets, then corrections, then adds.
#[must_use]
pub fn extract_nudge_actions(
    recent_messages: &[ConversationMessage],
    existing_memory: &PersistentMemory,
    config: &NudgeConfig,
) -> Vec<NudgeAction> {
    let mut actions = Vec::new();
    let max = config.max_entries_per_nudge;
    let now = now_ms();

    let mut scanned = 0usize;
    for msg in recent_messages.iter().rev() {
        if scanned >= config.lookback_turns {
            break;
        }
        if msg.role != MessageRole::User {
            continue;
        }
        scanned += 1;
        let text = collect_user_text(msg);
        if text.is_empty() {
            continue;
        }
        let lower = text.to_lowercase();

        // 1. Forget keywords — highest priority.
        for keyword in FORGET_KEYWORDS {
            if lower.contains(keyword) {
                let pattern = extract_after_phrase(&text, keyword).trim().to_string();
                if !pattern.is_empty() {
                    // Only emit Remove if an active entry actually matches
                    // the pattern — otherwise the action would be a no-op.
                    let matches = existing_memory
                        .entries()
                        .iter()
                        .any(|e| e.is_active(now) && e.content.contains(&pattern));
                    if matches {
                        actions.push(NudgeAction::Remove {
                            pattern,
                            source: "nudge-forget".to_string(),
                        });
                        if actions.len() >= max {
                            return actions;
                        }
                    }
                }
                break; // one forget keyword per message
            }
        }

        // 2. Corrections — second priority. Multiple correction phrases may
        //    fire on the same message (e.g. "No, I meant …" matches both
        //    "no, i meant" and "i meant"); break after the first match to
        //    avoid emitting duplicate actions for the same user turn.
        //    A correction also subsumes any later "remember"/"prefer"
        //    keyword in the same message — the correction phrase already
        //    captured the user's intent.
        let mut correction_emitted = false;
        for phrase in CORRECTION_PHRASES {
            if lower.contains(phrase) {
                let content = extract_after_phrase(&text, phrase).trim().to_string();
                if content.is_empty() {
                    continue;
                }
                let new_entry = MemoryEntry::new(content.clone(), "nudge-correction", now);
                let conflicts = existing_memory.detect_conflicts(&new_entry);
                if let Some(&idx) = conflicts.first() {
                    // Real prior fact found — supersede it.
                    let old_content = existing_memory.entries()[idx].content.clone();
                    actions.push(NudgeAction::Replace {
                        old_pattern: old_content,
                        new_content: content,
                        source: "nudge-correction".to_string(),
                    });
                } else {
                    // No prior fact to correct — treat as a fresh add.
                    actions.push(NudgeAction::Add {
                        content,
                        source: "nudge-correction".to_string(),
                    });
                }
                correction_emitted = true;
                if actions.len() >= max {
                    return actions;
                }
                break; // one correction per message
            }
        }
        if correction_emitted {
            continue; // next message — keyword pass subsumed by correction
        }

        // 3. Explicit memory keywords — lowest priority.
        for keyword in REMEMBER_KEYWORDS {
            if lower.contains(keyword) {
                let content = extract_after_phrase(&text, keyword).trim().to_string();
                if !content.is_empty() {
                    let new_entry = MemoryEntry::new(content.clone(), "nudge-keyword", now);
                    let conflicts = existing_memory.detect_conflicts(&new_entry);
                    if let Some(&idx) = conflicts.first() {
                        let old_content = existing_memory.entries()[idx].content.clone();
                        actions.push(NudgeAction::Replace {
                            old_pattern: old_content,
                            new_content: content,
                            source: "nudge-keyword".to_string(),
                        });
                    } else {
                        actions.push(NudgeAction::Add {
                            content,
                            source: "nudge-keyword".to_string(),
                        });
                    }
                    if actions.len() >= max {
                        return actions;
                    }
                }
                break; // one keyword per message
            }
        }
    }

    actions
}

/// Concatenate every text block from a user message into one string.
fn collect_user_text(message: &ConversationMessage) -> String {
    let mut buf = String::new();
    for block in &message.blocks {
        if let ContentBlock::Text { text } = block {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(text);
        }
    }
    buf
}

/// Return the substring following the first occurrence of `phrase`.
///
/// Used to lift the actionable content out of trigger phrases like
/// "remember I prefer dark mode" — returns "I prefer dark mode". Returns an
/// empty string when the phrase is not found.
fn extract_after_phrase(text: &str, phrase: &str) -> String {
    let lower = text.to_lowercase();
    let Some(start) = lower.find(phrase) else {
        return String::new();
    };
    let after = start + phrase.len();
    text.get(after..).unwrap_or("").trim_start_matches([':', ' ']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("claw-memory-{nanos}.json"))
    }

    // --- P2-7: PersistentMemory + Mid-Session Freeze ----------------------

    #[test]
    fn persistent_memory_load_and_freeze_creates_snapshot() {
        let path = temp_path();
        // Seed the file with one entry.
        {
            let mut mem = PersistentMemory::empty(&path);
            mem.add_entry("user prefers dark mode", "test-seed");
        }
        // Load should pick up the entry and freeze a snapshot containing it.
        let loaded = PersistentMemory::load_and_freeze(&path);
        let snapshot = loaded.frozen_render();
        assert!(snapshot.contains("# Persistent Memory"));
        assert!(snapshot.contains("user prefers dark mode"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn frozen_render_stable_within_session() {
        let path = temp_path();
        let mut mem = PersistentMemory::load_and_freeze(&path);
        let before = mem.frozen_render();
        // Mid-session mutation: write a new entry.
        mem.add_entry("user lives in Berlin", "session-test");
        let after = mem.frozen_render();
        // The frozen snapshot must NOT include the new entry — it was
        // captured at session start.
        assert_eq!(before, after, "frozen snapshot must stay stable within session");
        assert!(!after.contains("user lives in Berlin"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_captures_frozen_snapshot_for_byte_stability() {
        // B4 regression guard: `empty()` previously left frozen_snapshot
        // as None, so frozen_render() fell back to render_current() which
        // calls now_ms() — meaning two calls could return different
        // active-entries sets if an entry expired between them. The fix
        // captures a snapshot at construction time.
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        let before = mem.frozen_render();
        // Add an entry mid-session — snapshot must NOT pick it up.
        mem.add_entry("user likes rust", "test");
        let after = mem.frozen_render();
        assert_eq!(before, after, "empty() must capture a stable snapshot");
        assert!(!after.contains("user likes rust"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn add_entry_writes_to_disk_but_not_frozen_snapshot() {
        let path = temp_path();
        let mut mem = PersistentMemory::load_and_freeze(&path);
        mem.add_entry("likes rust", "test");
        // Frozen snapshot unchanged.
        assert!(!mem.frozen_render().contains("likes rust"));
        // But the disk file holds the new entry — reload to verify.
        let reloaded = PersistentMemory::load_and_freeze(&path);
        assert!(reloaded.frozen_render().contains("likes rust"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn needs_consolidation_triggers_at_80_percent() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        // Persona block has a 500-char budget; fill 450 (90%).
        let big: String = "x".repeat(450);
        mem.blocks_mut()[0].replace_content(big);
        assert!(mem.needs_consolidation());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn consolidate_reduces_entry_count() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("prefer dark mode", "s1");
        // Add a duplicate (will be deduped).
        mem.add_entry("prefer dark mode", "s2");
        // Add a distinct entry.
        mem.add_entry("works at acme", "s3");
        // Supersede one entry by direct manipulation.
        let now = now_ms();
        mem.entries[0].supersede("prefer light mode", now);
        let before = mem.entries().len();
        assert_eq!(before, 3);
        mem.consolidate();
        // Superseded entry is gone from `entries` (moved to `archive`),
        // duplicate is deduped, distinct entry stays.
        assert!(mem.entries().len() < before);
        assert!(mem.entries().iter().any(|e| e.content == "works at acme"));
        // B2 regression guard: superseded entry must be retained in `archive`
        // for audit purposes — NOT dropped on the floor.
        assert!(
            mem.archive().iter().any(|e| e.content == "prefer dark mode"),
            "superseded entries should be retained in archive: {:?}",
            mem.archive()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn consolidate_caps_archive_size() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        // Push well over ARCHIVE_MAX_ENTRIES superseded entries.
        for i in 0..(ARCHIVE_MAX_ENTRIES + 50) {
            mem.add_entry(&format!("stale fact {i}"), "s");
            let idx = mem.entries.len() - 1;
            mem.entries[idx].supersede("newer", now_ms());
        }
        mem.consolidate();
        assert!(
            mem.archive().len() <= ARCHIVE_MAX_ENTRIES,
            "archive should be capped at ARCHIVE_MAX_ENTRIES, got {}",
            mem.archive().len()
        );
        let _ = std::fs::remove_file(&path);
    }

    // --- P1-5: Typed Memory Blocks ---------------------------------------

    #[test]
    fn memory_blocks_persona_human_tasks_order() {
        let mem = PersistentMemory::empty(temp_path());
        let rendered = mem.render_blocks();
        let persona_pos = rendered.find("## Persona").expect("Persona label");
        let human_pos = rendered.find("## Human").expect("Human label");
        let tasks_pos = rendered.find("## Tasks").expect("Tasks label");
        assert!(persona_pos < human_pos);
        assert!(human_pos < tasks_pos);
    }

    #[test]
    fn single_block_over_capacity_only_compresses_that_block() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        // Overflow only the Persona block (500 chars).
        let big: String = "y".repeat(700);
        mem.blocks_mut()[0].replace_content(big.clone());
        mem.consolidate();
        // Persona got compressed.
        assert!(
            mem.blocks()[0].content().chars().count() <= mem.blocks()[0].max_chars() + 40,
            "Persona should be compressed within budget + marker"
        );
        assert!(mem.blocks()[0].content().contains("compressed"));
        // Human and Tasks are still empty (not touched).
        assert_eq!(mem.blocks()[1].content(), "");
        assert_eq!(mem.blocks()[2].content(), "");
        let _ = std::fs::remove_file(&path);
    }

    // --- P2-9: Temporal validity + conflict detection --------------------

    #[test]
    fn superseded_entries_excluded_from_active() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("prefer dark mode", "s1");
        let now = now_ms();
        mem.entries[0].supersede("prefer light mode", now);
        mem.add_entry("prefer light mode", "s2");
        let active: Vec<&MemoryEntry> = mem.entries().iter().filter(|e| e.is_active(now)).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].content, "prefer light mode");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expired_entries_excluded_from_active() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("temporary fact", "s1");
        let now = now_ms();
        // Mark as expired in the past.
        mem.entries[0].valid_until = Some(now - 1000);
        let active: Vec<&MemoryEntry> = mem.entries().iter().filter(|e| e.is_active(now)).collect();
        assert!(active.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn conflict_detection_finds_contradictory_entries() {
        let existing = vec![
            MemoryEntry::new("user prefers dark mode", "session-a", 1000),
            MemoryEntry::new("user likes rust", "session-a", 1100),
        ];
        let new = MemoryEntry::new("user prefers light mode", "session-b", 2000);
        let conflicts = detect_conflicts(&new, &existing);
        assert_eq!(conflicts, vec![0]);
    }

    #[test]
    fn conflict_detection_finds_chinese_contradictory_entries() {
        // B7 regression guard: previously the regex was ASCII-only, so
        // Chinese preferences like 偏好深色模式 vs 偏好浅色模式 silently
        // bypassed conflict detection and could coexist on disk.
        let existing = vec![
            MemoryEntry::new("用户偏好深色模式", "session-a", 1000),
            MemoryEntry::new("用户喜欢 rust", "session-a", 1100),
        ];
        let new = MemoryEntry::new("用户偏好浅色模式", "session-b", 2000);
        let conflicts = detect_conflicts(&new, &existing);
        assert_eq!(conflicts, vec![0]);
    }

    #[test]
    fn extract_keyword_value_chinese_extracts_preference() {
        let (kw, val) = extract_keyword_value("用户偏好深色模式")
            .expect("Chinese preference should extract");
        assert_eq!(kw, "偏好");
        assert!(val.contains("深色模式"));
    }

    #[test]
    fn add_entry_mirrors_into_semantic_l1_index() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("user prefers rust for systems programming", "test");
        // L1 index should have one entry that recall can match.
        let hits = mem.semantic_recall("rust systems programming", 5);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.summary.contains("rust"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_and_freeze_rebuilds_semantic_l1_index_from_entries() {
        let path = temp_path();
        {
            let mut mem = PersistentMemory::empty(&path);
            mem.add_entry("user prefers dark mode", "seed-1");
            mem.add_entry("user likes rust language", "seed-2");
        }
        // Reload — the in-memory semantic field is `#[serde(skip)]` so it
        // must be rebuilt from entries, otherwise recall returns nothing.
        let mem = PersistentMemory::load_and_freeze(&path);
        let hits = mem.semantic_recall("rust language", 5);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.summary.contains("rust"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unverified_entries_marked_after_7_days() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("old fact", "s1");
        // Backdate last_verified_at by 8 days.
        let now = now_ms();
        mem.entries[0].last_verified_at = now - (UNVERIFIED_THRESHOLD_MS + 1000);
        let active: Vec<&MemoryEntry> = mem.entries().iter().filter(|e| e.is_active(now)).collect();
        let rendered = render_entries(&active, now);
        assert!(rendered.contains("[unverified] old fact"));
        let _ = std::fs::remove_file(&path);
    }

    // --- P3-11: Periodic nudge ------------------------------------------

    #[test]
    fn should_nudge_returns_true_after_interval() {
        let cfg = NudgeConfig::default();
        assert!(!should_nudge(cfg.interval_turns - 1, &cfg));
        assert!(should_nudge(cfg.interval_turns, &cfg));
        assert!(should_nudge(cfg.interval_turns + 5, &cfg));
    }

    #[test]
    fn extract_nudge_actions_detects_remember_keyword() {
        let mem = PersistentMemory::empty(temp_path());
        let cfg = NudgeConfig::default();
        let msgs = vec![ConversationMessage::user_text(
            "Remember I prefer dark mode for code review",
        )];
        let actions = extract_nudge_actions(&msgs, &mem, &cfg);
        assert!(!actions.is_empty());
        match &actions[0] {
            NudgeAction::Add { content, .. } => {
                assert!(content.contains("dark mode"));
            }
            other => panic!("expected Add action, got {other:?}"),
        }
    }

    #[test]
    fn extract_nudge_actions_detects_correction() {
        // Pre-seed a contradicting fact so the correction has a real
        // target to supersede. With no prior fact, the correction would
        // fall back to Add (no prior fact to correct).
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("user prefers dark mode", "seed");
        let cfg = NudgeConfig::default();
        let msgs = vec![ConversationMessage::user_text(
            "No, I meant I prefer light mode",
        )];
        let actions = extract_nudge_actions(&msgs, &mem, &cfg);
        assert!(!actions.is_empty());
        match &actions[0] {
            NudgeAction::Replace {
                old_pattern,
                new_content,
                ..
            } => {
                assert!(new_content.contains("light mode"));
                // old_pattern must point at the real prior entry content,
                // NOT at the trigger phrase "no, i meant" (B1 regression guard).
                assert!(
                    old_pattern.contains("dark mode"),
                    "old_pattern should reference the superseded fact, got: {old_pattern}"
                );
                assert_ne!(old_pattern, "no, i meant");
            }
            other => panic!("expected Replace action, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extract_nudge_actions_correction_without_prior_fact_falls_back_to_add() {
        // No prior fact to correct — should produce Add, not Replace
        // with a meaningless old_pattern pointing at the trigger phrase.
        let mem = PersistentMemory::empty(temp_path());
        let cfg = NudgeConfig::default();
        let msgs = vec![ConversationMessage::user_text(
            "No, I meant I prefer light mode",
        )];
        let actions = extract_nudge_actions(&msgs, &mem, &cfg);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            NudgeAction::Add { content, .. } => {
                assert!(content.contains("light mode"));
            }
            other => panic!("expected Add action for correction-without-prior-fact, got {other:?}"),
        }
    }

    #[test]
    fn extract_nudge_actions_keyword_supersedes_contradicting_prior_entry() {
        // "remember I prefer light mode" while an existing entry says
        // "user prefers dark mode" — should produce Replace, not a
        // second contradicting Add.
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("user prefers dark mode", "seed");
        let cfg = NudgeConfig::default();
        let msgs = vec![ConversationMessage::user_text(
            "Remember I prefer light mode",
        )];
        let actions = extract_nudge_actions(&msgs, &mem, &cfg);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            NudgeAction::Replace { old_pattern, new_content, .. } => {
                assert!(old_pattern.contains("dark mode"));
                assert!(new_content.contains("light mode"));
            }
            other => panic!("expected Replace for contradicting keyword, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extract_nudge_actions_forget_keyword_emits_remove_when_entry_exists() {
        // B8 regression guard: previously NudgeAction::Remove was a dead
        // branch — extract never emitted it and conversation.rs silently
        // skipped the match arm. Now forget keywords emit Remove and
        // remove_entry retires the matching active entry into archive.
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("user likes rust", "seed");
        let cfg = NudgeConfig::default();
        let msgs = vec![ConversationMessage::user_text("forget rust")];
        let actions = extract_nudge_actions(&msgs, &mem, &cfg);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            NudgeAction::Remove { pattern, .. } => {
                assert!(pattern.contains("rust"));
            }
            other => panic!("expected Remove for forget keyword, got {other:?}"),
        }
        // Apply the action — entry should be retired into archive.
        for action in actions {
            if let NudgeAction::Remove { pattern, .. } = action {
                assert!(mem.remove_entry(&pattern), "remove_entry should retire matching entry");
            }
        }
        let now = now_ms();
        assert!(
            mem.entries().iter().all(|e| !e.is_active(now) || !e.content.contains("rust")),
            "rust entry should no longer be active"
        );
        assert!(
            mem.archive().iter().any(|e| e.content.contains("rust")),
            "retired entry should be retained in archive"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extract_nudge_actions_forget_keyword_no_match_emits_nothing() {
        // forget with no matching active entry should not emit Remove
        // (avoids no-op actions clogging the nudge budget).
        let mem = PersistentMemory::empty(temp_path());
        let cfg = NudgeConfig::default();
        let msgs = vec![ConversationMessage::user_text("forget non-existent thing")];
        let actions = extract_nudge_actions(&msgs, &mem, &cfg);
        assert!(actions.is_empty(), "expected no actions for unmatched forget");
    }

    #[test]
    fn extract_nudge_actions_respects_max_entries() {
        let mem = PersistentMemory::empty(temp_path());
        let cfg = NudgeConfig {
            interval_turns: 1,
            lookback_turns: 5,
            max_entries_per_nudge: 2,
        };
        let msgs = vec![
            ConversationMessage::user_text("Remember I like rust"),
            ConversationMessage::user_text("Remember I like go"),
            ConversationMessage::user_text("Remember I like python"),
        ];
        let actions = extract_nudge_actions(&msgs, &mem, &cfg);
        assert_eq!(actions.len(), cfg.max_entries_per_nudge);
    }
}
