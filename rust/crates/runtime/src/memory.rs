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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentMemory {
    #[serde(default = "default_blocks")]
    blocks: [MemoryBlock; 3],
    #[serde(default)]
    entries: Vec<MemoryEntry>,
    /// Path of the on-disk JSON file. Not serialized.
    #[serde(skip)]
    file_path: PathBuf,
    /// Snapshot captured at session start. Not serialized.
    #[serde(skip)]
    frozen_snapshot: Option<String>,
}

impl PersistentMemory {
    /// Build an empty memory surface pointing at the given file path.
    #[must_use]
    pub fn empty(file_path: impl Into<PathBuf>) -> Self {
        Self {
            blocks: default_blocks(),
            entries: Vec::new(),
            file_path: file_path.into(),
            frozen_snapshot: None,
        }
    }

    /// Load the memory file from disk and freeze a snapshot for the session.
    ///
    /// If the file does not exist or is empty, an empty memory is created.
    /// The snapshot is captured immediately after load so the prompt-cache
    /// prefix remains stable for the lifetime of the session, regardless of
    /// any in-memory mutations.
    pub fn load_and_freeze(file_path: &Path) -> Self {
        let mut memory = match MemoryStore::new(file_path.to_path_buf()).load() {
            Ok(Some(loaded)) => loaded.with_file_path(file_path.to_path_buf()),
            Ok(None) => Self::empty(file_path),
            // Defensive: corrupted file should not nuke the session. Log a
            // fresh memory and proceed — the next successful save will
            // overwrite the bad file.
            Err(_) => Self::empty(file_path),
        };
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

    /// Borrow all stored entries (including superseded / expired ones).
    #[must_use]
    pub fn entries(&self) -> &[MemoryEntry] {
        &self.entries
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
    /// Does NOT mutate `frozen_snapshot` — the snapshot is the only view
    /// surfaced to the system prompt within the current session, so new
    /// entries only appear in the next session.
    pub fn add_entry(&mut self, content: &str, source: &str) {
        let now = now_ms();
        self.entries
            .push(MemoryEntry::new(content.to_string(), source.to_string(), now));
        let _ = self.persist();
    }

    /// Replace the first entry whose content matches `old_content_pattern`
    /// with `new_content`, marking the old entry as superseded.
    ///
    /// If no match is found, the new content is still appended as a fresh
    /// entry — replacement is a no-op on the entries list, but the new fact
    /// still lands on disk.
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
        let _ = self.persist();
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
    /// Drops superseded and expired entries, deduplicates entries with
    /// identical content (keeping the newest), and compresses any block
    /// that has crossed its `max_chars` budget. The frozen snapshot is NOT
    /// touched — consolidation only affects future sessions.
    pub fn consolidate(&mut self) {
        // 1. Drop superseded / expired entries entirely.
        let now = now_ms();
        self.entries.retain(|entry| entry.is_active(now));

        // 2. Deduplicate by content, keeping the latest occurrence.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut deduped: Vec<MemoryEntry> = Vec::new();
        for entry in self.entries.iter().rev() {
            if seen.insert(entry.content.clone()) {
                deduped.push(entry.clone());
            }
        }
        deduped.reverse();
        self.entries = deduped;

        // 3. Compress any block that has crossed its budget.
        for block in &mut self.blocks {
            if block.is_over_capacity() {
                compress_block_content(block);
            }
        }

        let _ = self.persist();
    }

    /// Persist the current in-memory state to disk.
    fn persist(&self) -> std::io::Result<()> {
        MemoryStore::new(self.file_path.clone()).save(self)
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
            let _ = self.persist();
        }
    }
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

/// Return a compiled regex for detecting memory keywords with optional
/// conjugation suffixes. Compiled once and cached for the process lifetime.
fn contradiction_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(prefer|always|never|use|like|hate)(?:s|ed|d)?\b\s+(.+?)(?:[.,;\n]|$)",
        )
        .expect("contradiction regex should compile")
    })
}

/// Extract the (keyword, value) pair from a sentence like
/// "user prefers dark mode" → ("prefer", "dark mode").
///
/// Returns `None` when no memory keyword is found or the captured value is
/// empty.
fn extract_keyword_value(text: &str) -> Option<(String, String)> {
    let re = contradiction_regex();
    let caps = re.captures(text)?;
    let kw = caps.get(1)?.as_str().to_lowercase();
    let val = caps.get(2)?.as_str().trim().to_lowercase();
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

/// Extract candidate curation actions from the most recent conversation turns.
///
/// Rule-based only — no LLM fork. Two passes:
/// 1. Scan for correction phrases (`no, I meant`, `actually`, …). These
///    produce [`NudgeAction::Replace`] suggestions so the old fact is
///    superseded rather than contradicted on disk.
/// 2. Scan for explicit memory keywords (`remember`, `prefer`, `always`,
///    `never`, …). These produce [`NudgeAction::Add`] suggestions.
///
/// At most `config.max_entries_per_nudge` actions are returned, prioritising
/// corrections over adds.
#[must_use]
pub fn extract_nudge_actions(
    recent_messages: &[ConversationMessage],
    _existing_memory: &PersistentMemory,
    config: &NudgeConfig,
) -> Vec<NudgeAction> {
    let mut actions = Vec::new();
    let max = config.max_entries_per_nudge;

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

        // 1. Corrections first — higher priority.
        for phrase in CORRECTION_PHRASES {
            if lower.contains(phrase) {
                let content = extract_after_phrase(&text, phrase).trim().to_string();
                if !content.is_empty() {
                    actions.push(NudgeAction::Replace {
                        old_pattern: phrase.to_string(),
                        new_content: content,
                        source: "nudge-correction".to_string(),
                    });
                    if actions.len() >= max {
                        return actions;
                    }
                }
            }
        }

        // 2. Explicit memory keywords.
        for keyword in REMEMBER_KEYWORDS {
            if lower.contains(keyword) {
                let content = extract_after_phrase(&text, keyword).trim().to_string();
                if !content.is_empty() {
                    actions.push(NudgeAction::Add {
                        content,
                        source: "nudge-keyword".to_string(),
                    });
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
        // Superseded entry is gone, duplicate is gone, distinct entry stays.
        assert!(mem.entries().len() < before);
        assert!(mem.entries().iter().any(|e| e.content == "works at acme"));
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
        let mem = PersistentMemory::empty(temp_path());
        let cfg = NudgeConfig::default();
        let msgs = vec![ConversationMessage::user_text(
            "No, I meant I prefer light mode",
        )];
        let actions = extract_nudge_actions(&msgs, &mem, &cfg);
        assert!(!actions.is_empty());
        match &actions[0] {
            NudgeAction::Replace { new_content, .. } => {
                assert!(new_content.contains("light mode"));
            }
            other => panic!("expected Replace action, got {other:?}"),
        }
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
