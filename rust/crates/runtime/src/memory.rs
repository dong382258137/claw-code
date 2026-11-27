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

use crate::memory_extract::{Basis, Confidence, EntryKind, MemoryOp, OpKind};
use crate::memory_semantic::SemanticRecaller;
use crate::memory_store::MemoryStore;
use crate::memory_taxonomy::{classify_subject, MemoryTaxonomy};

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

/// `memory.json` 所在目录（词表 / 候选 / journal 都放在这里）。
fn state_dir_for(file_path: &Path) -> &Path {
    file_path.parent().unwrap_or_else(|| Path::new("."))
}

/// 180 天未被召回 / 验证的活跃条目降级归档（R4）。
pub const ACTIVE_MAX_AGE_MS: i64 = 180 * 24 * 60 * 60 * 1000;

/// 活跃条目硬上限（R4）。超出后按最后触碰时间淘汰最旧条目。
pub const ACTIVE_MAX_ENTRIES: usize = 100;

/// Derive a stable L1 index id from entry content. Same content → same id,
/// so duplicate entries collapse to a single L1 slot during dedup. Uses
/// the std default hasher (no extra dependency) — collision resistance
/// is sufficient for an in-memory index that is rebuilt on every load.
pub(crate) fn entry_id(content: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("entry-{:016x}", hasher.finish())
}

/// Shadow entry marker. Every injected view (projection / semantic recall)
/// must carry it so neither the model nor the user mistakes an unconfirmed
/// shadow memory for a confirmed one.
pub const SHADOW_PREFIX: &str = "[影子·未确认]";

/// Render label for a shadow entry (marker + content).
fn shadow_label(content: &str) -> String {
    format!("{SHADOW_PREFIX} {content}")
}

/// Stable id for a shadow entry (derived from the labelled content).
fn shadow_entry_id(content: &str) -> String {
    entry_id(&shadow_label(content))
}

/// One of the three typed memory blocks mirroring Letta's core memory model.
///
/// Each variant carries its own `max_chars` budget so the renderer can keep
/// the Persona / Human / Tasks prefix stable independently — overflowing a
/// single block only compresses that block, never the others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MemoryBlock {
    Persona { content: String, max_chars: usize },
    Human { content: String, max_chars: usize },
    Tasks { content: String, max_chars: usize },
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
    [
        MemoryBlock::persona(),
        MemoryBlock::human(),
        MemoryBlock::tasks(),
    ]
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
    /// 记忆类别（Phase 1：`UserProfile` 用于 Human 只读投影）。
    #[serde(default)]
    pub kind: EntryKind,
    /// 归一化主体（如 `editor` / `user`），用于 subject+attribute 冲突判定。
    #[serde(default)]
    pub subject: Option<String>,
    /// 归一化属性（如 `indent_style`）。
    #[serde(default)]
    pub attribute: Option<String>,
    /// 提取器给出的置信度。
    #[serde(default)]
    pub confidence: Confidence,
    /// 事实来源：用户自述 / 推断 / 约束。
    #[serde(default)]
    pub basis: Basis,
    /// 该事实累计出现次数（二现门槛）。
    #[serde(default)]
    pub evidence_count: u32,
    /// 最后一次被召回命中的时间（ms），供 180 天降级判定。
    #[serde(default)]
    pub last_recalled_at: Option<i64>,
    /// 词表迁移时无法映射的 subject 被标为孤儿（保留原值供人工审阅）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub orphan: bool,
    /// 孤儿条目的原始 subject 文本（迁移前），仅用于审阅。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_subject: Option<String>,
}

/// `skip_serializing_if` helper：`false` 不落盘，保持 `memory.json` 干净。
fn is_false(value: &bool) -> bool {
    !*value
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
            kind: EntryKind::Fact,
            subject: None,
            attribute: None,
            confidence: Confidence::Medium,
            basis: Basis::Inferred,
            evidence_count: 1,
            last_recalled_at: None,
            orphan: false,
            legacy_subject: None,
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
/// ever lands in the system prompt prefix within that session — this keeps
/// the prompt-cache prefix stable. New facts written mid-session (via nudge /
/// [`PersistentMemory::add_entry`]) land on disk and are mirrored into the
/// semantic L1 index **immediately**, so they surface in the current session
/// through the semantic-recall channel (appended to the prompt's dynamic
/// tail, not the frozen prefix); they additionally surface in the frozen
/// prefix from the next session onward.
///
/// `entries` holds only currently-active facts. Superseded / expired
/// entries are migrated to `archive` during [`PersistentMemory::consolidate`]
/// so audit history is preserved without bloating the active view.
/// 一条撤回记录：按 subject+attribute 拦截重新写入（A4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetractedSubject {
    pub subject: String,
    /// `None` 表示该 subject 下所有 attribute 一律拦截。
    pub attribute: Option<String>,
    pub retracted_at: i64,
}

// Step 2.4: Eq derive 移除 — SemanticRecaller 持有 `HashMap<String, Vec<f32>>` 向量索引,
// `Vec<f32>` 不 impl Eq(floating-point NaN 问题),导致 SemanticRecaller 也不能 impl Eq。
// PartialEq 保留,测试用 `assert_eq!` 比较字段而非整体 struct 不受影响。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// 已撤回的 subject+attribute（A4：独立集合，不进 entries，避免内容哈希
    /// 不一致导致去重失效）。
    #[serde(default)]
    retracted_subjects: Vec<RetractedSubject>,
    /// 旧 block 是否已一次性迁移到 entries（R5：迁移后写入冻结）。
    #[serde(default)]
    blocks_migrated: bool,
    /// Path of the on-disk JSON file. Not serialized.
    #[serde(skip)]
    file_path: PathBuf,
    /// Snapshot captured at session start. Not serialized.
    #[serde(skip)]
    frozen_snapshot: Option<String>,
    /// 语义召回层 (L1 index)。Not serialized — rebuilt at
    /// load time from the entries list. Lives on the memory surface so
    /// the runtime can issue [`PersistentMemory::semantic_recall`] without
    /// juggling a second handle.
    #[serde(skip)]
    semantic: SemanticRecaller,
    /// `visible` 模式下的影子条目：进内存投影、可召回，但 `#[serde(skip)]`
    /// 保证永不写入 `memory.json`（重启即消失）。
    #[serde(skip)]
    shadow_entries: Vec<MemoryEntry>,
    /// 受控 subject/attribute 词表（由 `<memory.json 所在目录>` 配置加载，
    /// 不随 `memory.json` 序列化）。
    #[serde(skip)]
    taxonomy: MemoryTaxonomy,
}

/// Maximum number of archived (superseded / expired) entries to retain
/// on disk. Older entries are pruned during consolidation to bound growth.
pub const ARCHIVE_MAX_ENTRIES: usize = 200;

/// 一批 op 的应用结果（供 `/memory` 报告与埋点）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: usize,
    pub retracted: usize,
    pub skipped: usize,
    pub conflicts: usize,
    pub evicted: u64,
}

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
        let file_path = file_path.into();
        let taxonomy = MemoryTaxonomy::load(state_dir_for(&file_path));
        let mut memory = Self {
            blocks: default_blocks(),
            entries: Vec::new(),
            archive: Vec::new(),
            retracted_subjects: Vec::new(),
            blocks_migrated: false,
            file_path,
            frozen_snapshot: None,
            semantic: SemanticRecaller::new(),
            shadow_entries: Vec::new(),
            taxonomy,
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
        // 词表加载 + 旧数据迁移：自由文本 subject 映射到枚举，映射不了的标孤儿。
        // 孤儿通过 `/memory` 面板展示（见 orphan_entries），这里不重复刷日志。
        memory.taxonomy = MemoryTaxonomy::load(state_dir_for(file_path));
        let _ = memory.migrate_taxonomy();
        // R5:旧 Persona/Human/Tasks 块一次性迁移到 entries，之后 block 写入冻结。
        memory.migrate_blocks_once();
        // 崩溃回放：apply 只写内存 + 追加日志，全量快照在边界落盘后才清空日志。
        // 回放是幂等的（内容去重 + subject supersede），重复回放不会产生重复条目。
        let pending = crate::memory_extract::load_pending_ops(
            &crate::memory_extract::journal_path_for(file_path),
        );
        if !pending.is_empty() {
            memory.apply_ops(&pending, ACTIVE_MAX_ENTRIES);
        }
        // R4:启动时执行遗忘规则（180 天降级 + active 上限）。
        let now = now_ms();
        memory.enforce_limits(now, ACTIVE_MAX_ENTRIES);
        // Rebuild semantic L1 index from entries. Only active entries are
        // indexed — superseded / expired ones are in `archive` and not
        // useful for recall. Orphans are excluded for the same reason they
        // are excluded from rendering: their classification is undecided,
        // so recalling them surfaces unmanaged noise into the prompt's
        // dynamic region.
        let mut recaller = SemanticRecaller::new();
        for entry in &memory.entries {
            if entry.is_active(now) && !entry.orphan {
                recaller.add_l1_entry(&entry_id(&entry.content), &entry.content, &entry.source);
            }
        }
        memory.semantic = recaller;
        // Step 4.x:注入全局 embedding provider(若可用),使 semantic_recall
        // 自动使用向量搜索而非退化为 keyword。
        if let Some(provider) = crate::build_embedding_provider() {
            let mut recaller =
                std::mem::take(&mut memory.semantic).with_embedding_provider(provider.clone());
            // 语义记忆接线:load 时批量预计算全部 entry 的向量。
            // 没有这一步 vectors 索引恒为空,向量召回永远不触发,
            // `semantic_recall` 实际退化为 keyword 子串匹配。
            // index_embeddings 幂等且失败时保留已计算部分,不阻塞加载。
            match recaller.index_embeddings(provider.as_ref()) {
                Ok(count) if count > 0 => {
                    eprintln!("[memory] indexed {count} memory entr(ies) for semantic recall");
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[memory] embedding index failed ({e}); semantic recall falls back to keyword");
                }
            }
            memory.semantic = recaller;
        }
        memory.frozen_snapshot = Some(memory.render_current());
        memory
    }

    /// Set the on-disk file path (used when restoring after load).
    fn with_file_path(mut self, path: PathBuf) -> Self {
        self.file_path = path;
        self
    }

    /// 把既有条目的自由文本 subject 迁移到受控词表。
    ///
    /// 能映射的改成规范 subject / attribute；映射不了的把原值保留到
    /// `legacy_subject` 并标 `orphan = true`，不静默当正常条目继续使用。
    /// 返回被标记为孤儿的条目数（活跃 + 归档）。
    fn migrate_taxonomy(&mut self) -> usize {
        let taxonomy = self.taxonomy.clone();
        let mut orphans = 0usize;
        for entry in self.entries.iter_mut().chain(self.archive.iter_mut()) {
            let Some(original) = entry.subject.clone() else {
                continue;
            };
            // 孤儿条目优先用迁移前原文重新解析：用户以后补词表类目时可自动归位。
            let raw = if entry.orphan {
                entry.legacy_subject.clone().unwrap_or(original)
            } else {
                original
            };
            let (subject, is_orphan) =
                classify_subject(&taxonomy, Some(&raw), entry.attribute.as_deref());
            if let Some(subject) = subject.as_deref() {
                entry.attribute = taxonomy.normalize_attribute(subject, entry.attribute.as_deref());
            }
            entry.subject = subject;
            if is_orphan {
                // 只在首次标记时计数，避免每轮 load 重复刷日志。
                if !entry.orphan {
                    orphans += 1;
                }
                entry.orphan = true;
                if entry.legacy_subject.is_none() {
                    entry.legacy_subject = Some(raw);
                }
            } else {
                // 词表补了类目后可以自动归位，不再算孤儿。
                entry.orphan = false;
            }
        }
        orphans
    }

    /// 孤儿条目（subject 不在词表，需人工审阅；`/memory` 会给出计数）。
    #[must_use]
    pub fn orphan_entries(&self) -> Vec<&MemoryEntry> {
        self.entries
            .iter()
            .chain(self.archive.iter())
            .filter(|entry| entry.orphan)
            .collect()
    }

    /// 当前生效的 subject 词表（诊断 / 测试用）。
    #[must_use]
    pub fn taxonomy(&self) -> &MemoryTaxonomy {
        &self.taxonomy
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

    /// Update one of the three typed blocks (Persona / Human / Tasks) in
    /// place and persist to disk.
    ///
    /// `label` is matched case-insensitively against the block labels
    /// (`persona` / `human` / `tasks`, case-insensitive). If the new content
    /// exceeds the block's `max_chars` budget it is truncated to the budget
    /// with a trailing `…` marker.
    ///
    /// The frozen snapshot is NOT mutated — the new block content joins the
    /// system-prompt prefix from the next session onward (mid-session the
    /// prefix stays byte-stable to preserve prompt-cache hits). The current
    /// session observes the write through the returned confirmation text.
    ///
    /// Returns an error for unknown labels.
    pub fn update_block(&mut self, label: &str, content: &str) -> Result<String, String> {
        let normalized = label.trim().to_ascii_lowercase();
        if !matches!(normalized.as_str(), "persona" | "human" | "tasks") {
            return Err(format!(
                "unknown memory block '{label}': expected one of persona / human / tasks"
            ));
        }
        // R5:迁移后 block 写入冻结 —— 统一写入 entries（Human 由只读投影渲染），
        // 避免块与条目形成两个真相源。
        if self.blocks_migrated {
            let op = MemoryOp {
                op: OpKind::Upsert,
                kind: if normalized == "human" {
                    EntryKind::UserProfile
                } else {
                    EntryKind::Fact
                },
                subject: Some(normalized.clone()),
                attribute: None,
                content: content.trim().to_string(),
                confidence: Confidence::High,
                source: "memory_update:block".to_string(),
                explicit: true,
                basis: Basis::UserStated,
                ..MemoryOp::default()
            };
            let report = self.apply_ops(std::slice::from_ref(&op), ACTIVE_MAX_ENTRIES);
            // turn 路径不落全量快照：写入追加日志，边界 flush 后清空。
            crate::memory_extract::append_journal(&self.file_path, std::slice::from_ref(&op));
            return Ok(format!(
                "memory block '{normalized}' 已冻结，写入 entries（Human 为只读投影）；applied={} skipped={}.",
                report.applied, report.skipped
            ));
        }
        // 在块借用期间完成替换,提取标量结果后结束借用,再持久化(避免
        // block 可变借用与 persist_or_warn 的 &self 冲突)。
        let (block_label, max, written, truncated) = {
            let Some(block) = self
                .blocks
                .iter_mut()
                .find(|block| block.label().to_ascii_lowercase() == normalized)
            else {
                return Err(format!(
                    "unknown memory block '{label}': expected one of persona / human / tasks"
                ));
            };
            let max = block.max_chars();
            let mut content = content.to_string();
            let truncated = content.chars().count() > max;
            if truncated {
                content = content.chars().take(max).collect::<String>() + "…";
            }
            let written = content.chars().count();
            let block_label = block.label();
            block.replace_content(content);
            (block_label, max, written, truncated)
        };
        self.persist_or_warn("update_block");
        Ok(format!(
            "memory block '{block_label}' updated ({written} chars, budget {max}{}).",
            if truncated {
                ", truncated to budget"
            } else {
                ""
            },
        ))
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

    /// 是否有可注入内容(任一类型块非空,或存在活跃条目)。
    ///
    /// 全空时注入端应省略整段(P1-5a):旧行为无条件注入
    /// `## Persona\n_(empty)_ / ## Human / ## Tasks` 占位,每轮浪费 ~100 token
    /// 且制造"空槽位噪音"。省略整段后缓存前缀变化一次(会话启动捕获),
    /// 之后字节仍稳定。
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.blocks.iter().any(|b| !b.content().trim().is_empty())
            || !self.active_entries(now_ms()).is_empty()
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
        let blocks = self.render_blocks();
        if !blocks.is_empty() {
            sections.push(blocks);
        }
        // R5:Human 槽只做 entries 的只读投影；被投影命中的条目不再重复出现在
        // Active facts 段，避免同一条偏好注入两次。
        let projection = self.render_projection();
        if !projection.is_empty() {
            sections.push(projection);
        }
        // 孤儿条目不进注入前缀。
        //
        // `orphan = true` 表示该条目的 subject 无法映射到受控词表（见
        // [`PersistentMemory::migrate_taxonomy`]），即「这条记忆归哪一类」本身
        // 就没有定论。把它当正常事实每轮注入，等于用未分类的噪声占用缓存
        // 前缀——它既无法被 subject+attribute 覆盖语义管理（`apply_upsert`
        // 按这对键 supersede），也不会被 `/memory rm` 以外的路径清理。
        // 孤儿仍保留在 `entries` 里供 `/memory` 面板审阅与人工归位
        // （见 [`PersistentMemory::orphan_entries`]），只是不参与渲染。
        let active: Vec<&MemoryEntry> = self
            .active_entries(now_ms())
            .into_iter()
            .filter(|entry| entry.kind != EntryKind::UserProfile && !entry.orphan)
            .collect();
        if !active.is_empty() {
            sections.push(render_entries(&active, now_ms()));
        }
        sections.join("\n\n")
    }

    /// Human 槽的只读投影：仅由 `EntryKind::UserProfile` 的活跃条目生成。
    ///
    /// `visible` 模式的影子条目追加在 `## Shadow (unconfirmed)` 段，每条都带
    /// [`SHADOW_PREFIX`] 标记，不得静默混入已确认记忆。
    #[must_use]
    pub fn render_projection(&self) -> String {
        let now = now_ms();
        let mut lines = Vec::new();
        for entry in &self.entries {
            if entry.is_active(now) && entry.kind == EntryKind::UserProfile {
                lines.push(format!("- {}", entry.content));
            }
        }
        let mut out = if lines.is_empty() {
            String::new()
        } else {
            format!("## Human\n{}", lines.join("\n"))
        };
        if !self.shadow_entries.is_empty() {
            let mut shadow = String::from("## Shadow (unconfirmed)\n");
            for entry in &self.shadow_entries {
                shadow.push_str(&format!("- {}\n", shadow_label(&entry.content)));
            }
            if out.is_empty() {
                out = shadow.trim_end().to_string();
            } else {
                out.push_str("\n\n");
                out.push_str(shadow.trim_end());
            }
        }
        out
    }

    /// 影子条目（`visible` 模式；仅供 `/memory` 审阅与撤销）。
    #[must_use]
    pub fn shadow_entries(&self) -> &[MemoryEntry] {
        &self.shadow_entries
    }

    /// 撤销第 `index`（0 基）条影子条目；返回被移除的条目。
    pub fn remove_shadow_entry(&mut self, index: usize) -> Option<MemoryEntry> {
        if index < self.shadow_entries.len() {
            Some(self.shadow_entries.remove(index))
        } else {
            None
        }
    }

    /// 旧 Persona/Human/Tasks 块一次性迁移进 entries（R5），迁移后
    /// [`PersistentMemory::update_block`] 走冻结路径。
    ///
    /// 返回是否发生了迁移。失败时仍然置位 `blocks_migrated`，避免每次加载
    /// 重复尝试。
    pub fn migrate_blocks_once(&mut self) -> bool {
        if self.blocks_migrated {
            return false;
        }
        let now = now_ms();
        let mut migrated: Vec<MemoryEntry> = Vec::new();
        for block in &self.blocks {
            let label = block.label().to_ascii_lowercase();
            for line in block.content().lines() {
                let text = line.trim().trim_start_matches(['-', '*']).trim();
                if text.is_empty() {
                    continue;
                }
                let mut entry = MemoryEntry::new(text.to_string(), "block-migration", now);
                entry.kind = if label == "human" {
                    EntryKind::UserProfile
                } else {
                    EntryKind::Fact
                };
                entry.subject = Some(label.clone());
                entry.basis = Basis::UserStated;
                migrated.push(entry);
            }
        }
        for block in &mut self.blocks {
            block.replace_content(String::new());
        }
        self.entries.extend(migrated);
        self.blocks_migrated = true;
        self.persist_or_warn("migrate_blocks");
        true
    }

    /// Render the three typed blocks in fixed order (Persona → Human → Tasks).
    ///
    /// Empty blocks are skipped entirely: a `## X\n_(empty)_` slot carries no
    /// knowledge and costs tokens every turn. Order of non-empty blocks stays
    /// fixed, so the rendered bytes remain stable for the same input.
    #[must_use]
    pub fn render_blocks(&self) -> String {
        let mut sections = Vec::new();
        for block in &self.blocks {
            let label = block.label();
            let content = block.content();
            if !content.is_empty() {
                sections.push(format!("## {label}\n{content}"));
            }
        }
        sections.join("\n\n")
    }

    /// Append a new entry to the in-memory store and persist to disk.
    ///
    /// Also mirrors the new entry into the semantic L1 index so subsequent
    /// [`PersistentMemory::semantic_recall`] calls can surface it **in the
    /// current session** (the recall channel is appended to the prompt's
    /// dynamic tail, so this does not perturb the frozen prefix). The frozen
    /// snapshot is NOT mutated — the prefix view captured at session start
    /// stays byte-stable; the new entry joins that prefix from the next
    /// session onward.
    pub fn add_entry(&mut self, content: &str, source: &str) {
        let now = now_ms();
        self.entries.push(MemoryEntry::new(
            content.to_string(),
            source.to_string(),
            now,
        ));
        let entry_id = entry_id(content);
        self.semantic.add_l1_entry(&entry_id, content, source);
        // 语义记忆接线:新增 entry 增量计算向量,使其立即可被向量召回命中。
        // (load 时已有向量用批量 index_embeddings;运行时新增走增量 embed_entry。)
        self.semantic.embed_entry(&entry_id);
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
        self.entries.push(MemoryEntry::new(
            new_content.to_string(),
            source.to_string(),
            now,
        ));
        let entry_id = entry_id(new_content);
        self.semantic.add_l1_entry(&entry_id, new_content, source);
        // 语义记忆接线:replace 同样增量计算新 entry 向量。
        self.semantic.embed_entry(&entry_id);
        self.persist_or_warn("replace_entry");
    }

    /// 工具路径写入（`memory_update: add_entry`）：纯内存 apply + 追加 journal。
    ///
    /// 与自动抽取路径同一条路：不 embedding、不全量落盘。语义 L1 索引即时
    /// 更新（本会话关键词召回可见），向量留待下次加载时批量重建，全量持久化
    /// 交给会话边界 [`PersistentMemory::flush`]。journal 写入失败静默，工具
    /// 结果保持成功语义。
    pub fn record_entry(&mut self, content: &str, source: &str) -> ApplyReport {
        let op = MemoryOp {
            op: OpKind::Upsert,
            content: content.trim().to_string(),
            source: source.to_string(),
            confidence: Confidence::High,
            basis: Basis::UserStated,
            explicit: true,
            evidence_count: 1,
            ..MemoryOp::default()
        };
        self.apply_and_journal(&op)
    }

    /// 工具路径替换（`memory_update: replace_entry`）：按 `old_content_pattern`
    /// 退休旧条目后再插入新内容，其余同 [`PersistentMemory::record_entry`]。
    pub fn record_replacement(
        &mut self,
        old_content_pattern: &str,
        new_content: &str,
        source: &str,
    ) -> ApplyReport {
        let op = MemoryOp {
            op: OpKind::Upsert,
            content: new_content.trim().to_string(),
            source: source.to_string(),
            target: Some(old_content_pattern.to_string()),
            confidence: Confidence::High,
            basis: Basis::UserStated,
            explicit: true,
            evidence_count: 1,
            ..MemoryOp::default()
        };
        self.apply_and_journal(&op)
    }

    /// 工具路径删除（`memory_update: remove_entry`）：按 pattern 退休条目。
    ///
    /// 返回是否有条目被移除；未命中不写 journal（回放同样不产生变化）。
    pub fn retract_entry_by_pattern(&mut self, pattern: &str) -> bool {
        let op = MemoryOp {
            op: OpKind::Retract,
            target: Some(pattern.to_string()),
            source: "memory_update".to_string(),
            basis: Basis::UserStated,
            explicit: true,
            ..MemoryOp::default()
        };
        let report = self.apply_ops(std::slice::from_ref(&op), ACTIVE_MAX_ENTRIES);
        if report.retracted == 0 {
            return false;
        }
        crate::memory_extract::append_journal(&self.file_path, std::slice::from_ref(&op));
        true
    }

    /// 应用单条 op 并追加 journal。
    ///
    /// journal 写失败只影响崩溃兜底，不改变工具结果（调用方仍按已记录返回）。
    fn apply_and_journal(&mut self, op: &MemoryOp) -> ApplyReport {
        let report = self.apply_ops(std::slice::from_ref(op), ACTIVE_MAX_ENTRIES);
        if report.applied > 0 {
            crate::memory_extract::append_journal(&self.file_path, std::slice::from_ref(op));
        }
        report
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
        // 不再在 consolidate 内落盘：turn 路径禁止全量文件写，边界统一 flush。
    }

    /// 一批 op 的应用结果（供 `/memory` 报告与埋点）。
    /// 应用 worker 产出的 op（R2：**纯内存**，不做 embedding、不做全量落盘）。
    ///
    /// turn 路径上不允许出现全量文件写：持久化统一由
    /// [`PersistentMemory::flush`] 在会话边界执行，崩溃由追加日志回放兜底。
    ///
    /// 顺序：脱敏（调用方已做）→ L1 精确去重 → L3 同 subject Jaccard 去重 →
    /// subject+attribute 冲突 supersede → 旧式短语冲突（`detect_conflicts`）→
    /// 遗忘规则 → `consolidate`。
    pub fn apply_ops(&mut self, ops: &[MemoryOp], max_active: usize) -> ApplyReport {
        let now = now_ms();
        let mut report = ApplyReport::default();
        for op in ops {
            // 写入前统一归一化：worker、journal 回放、工具路径都走这里，
            // 保证 (subject, attribute) 在入库前已经落到受控词表。
            let mut op = op.clone();
            self.taxonomy.normalize_op(&mut op);
            let op = &op;
            if op.shadow {
                if self.apply_shadow_op(op, now, &mut report) {
                    report.applied += 1;
                }
                continue;
            }
            if op.op == OpKind::Retract {
                if self.apply_retract(op, now) {
                    report.retracted += 1;
                } else {
                    report.skipped += 1;
                }
                continue;
            }
            if self.apply_upsert(op, now, &mut report) {
                report.applied += 1;
            }
        }
        report.evicted = self.enforce_limits(now, max_active);
        // R3:遗忘（consolidate）与冲突检测（detect_conflicts）的原生产调用点
        // 在 nudge 块内；nudge 删除后由本入口接管，不得丢失。
        if report.evicted > 0 || self.needs_consolidation() {
            self.consolidate();
        }
        report
    }

    /// 影子 op：只进内存投影（可召回），不参与持久化。
    fn apply_shadow_op(&mut self, op: &MemoryOp, now: i64, report: &mut ApplyReport) -> bool {
        if op.op == OpKind::Retract {
            let before = self.shadow_entries.len();
            self.shadow_entries.retain(|entry| {
                let subject_hit = op
                    .subject
                    .as_deref()
                    .is_some_and(|subject| entry.subject.as_deref() == Some(subject));
                let target_hit = op
                    .target
                    .as_deref()
                    .is_some_and(|target| entry.content.contains(target));
                !(subject_hit || target_hit)
            });
            if self.shadow_entries.len() < before {
                report.retracted += 1;
                return true;
            }
            report.skipped += 1;
            return false;
        }
        let content = op.content.trim();
        if content.is_empty() || self.is_retracted(op.subject.as_deref(), op.attribute.as_deref()) {
            report.skipped += 1;
            return false;
        }
        let exact_dup = self
            .entries
            .iter()
            .chain(self.shadow_entries.iter())
            .any(|entry| entry.is_active(now) && entry.content == content);
        if exact_dup {
            report.skipped += 1;
            return false;
        }
        if let Some(subject) = op.subject.as_deref() {
            let near_dup = self
                .entries
                .iter()
                .chain(self.shadow_entries.iter())
                .any(|entry| {
                    entry.is_active(now)
                        && entry.subject.as_deref() == Some(subject)
                        && jaccard_similarity(&entry.content, content) > 0.95
                });
            if near_dup {
                report.skipped += 1;
                return false;
            }
        }
        let mut entry = MemoryEntry::new(content.to_string(), "shadow", now);
        entry.kind = op.kind;
        entry.subject = op.subject.clone();
        entry.attribute = op.attribute.clone();
        entry.confidence = op.confidence;
        entry.basis = op.basis;
        entry.evidence_count = op.evidence_count.max(1);
        if let (Some(subject), Some(attribute)) = (op.subject.as_deref(), op.attribute.as_deref()) {
            report.conflicts += self
                .entries
                .iter()
                .filter(|existing| {
                    existing.is_active(now)
                        && existing.subject.as_deref() == Some(subject)
                        && existing.attribute.as_deref() == Some(attribute)
                })
                .count();
        }
        // 影子条目也进 L1 索引，但内容带可辨识前缀，召回注入时人和模型都能看出
        // 它尚未被确认（`/memory rm` 可当场撤销）。
        self.semantic
            .add_l1_entry(&shadow_entry_id(content), &shadow_label(content), "shadow");
        self.shadow_entries.push(entry);
        true
    }

    fn apply_upsert(&mut self, op: &MemoryOp, now: i64, report: &mut ApplyReport) -> bool {
        let content = op.content.trim();
        if content.is_empty() {
            report.skipped += 1;
            return false;
        }
        if self.is_retracted(op.subject.as_deref(), op.attribute.as_deref()) {
            report.skipped += 1;
            return false;
        }
        // 显式替换（`memory_update: replace_entry`）：先退休所有命中 `target`
        // 的活跃条目，再按普通 upsert 插入新内容。走同一套 op 语义后，
        // journal 回放能复现同样的替换结果，无需另起一套替换逻辑。
        let explicit_replace = op.target.as_deref().is_some_and(|t| !t.is_empty());
        if explicit_replace {
            let target = op.target.as_deref().unwrap_or_default();
            let mut retired = 0usize;
            for entry in &mut self.entries {
                if entry.is_active(now) && entry.content.contains(target) {
                    entry.supersede(content.to_string(), now);
                    retired += 1;
                }
            }
            report.conflicts += retired;
        }
        // L1：内容完全相同的活跃条目 → 刷新验证时间，不重复插入。
        //
        // 同时回填元数据：条目可能是旧版本写的（`kind` 缺省为 `fact`、
        // `subject`/`attribute` 为 `None`）。若只刷新时间戳，这些条目永远
        // 停在旧形态 —— `kind != UserProfile` 就进不了 Human 只读投影，
        // `subject = None` 就参与不了 subject+attribute 冲突判定。
        // 只在**当前值为空/缺省**时回填，绝不覆盖已有的更具体取值。
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_active(now) && entry.content == content)
        {
            existing.last_verified_at = now;
            existing.evidence_count = existing.evidence_count.saturating_add(1);
            if existing.kind == EntryKind::Fact && op.kind != EntryKind::Fact {
                existing.kind = op.kind;
            }
            if existing.subject.is_none() {
                existing.subject = op.subject.clone();
            }
            if existing.attribute.is_none() {
                existing.attribute = op.attribute.clone();
            }
            if existing.basis == Basis::Inferred && op.basis != Basis::Inferred {
                existing.basis = op.basis;
            }
            return true;
        }
        // L3：同 subject 下语义近重复（>0.95）跳过，避免脑补改写。
        // 显式替换不复用该判定：模型已点名要换掉旧事实，近重复不应吞掉新内容。
        if let Some(subject) = op.subject.as_deref().filter(|_| !explicit_replace) {
            let near_dup = self.entries.iter().any(|entry| {
                entry.is_active(now)
                    && entry.subject.as_deref() == Some(subject)
                    && jaccard_similarity(&entry.content, content) > 0.95
            });
            if near_dup {
                report.skipped += 1;
                return false;
            }
        }

        let mut new_entry = MemoryEntry::new(content.to_string(), op.source.clone(), now);
        new_entry.kind = op.kind;
        new_entry.subject = op.subject.clone();
        new_entry.attribute = op.attribute.clone();
        new_entry.confidence = op.confidence;
        new_entry.basis = op.basis;
        new_entry.evidence_count = op.evidence_count.max(1);

        // subject+attribute 命中 → 旧条目 supersede（upsert 语义）。
        let mut conflicts: Vec<usize> = Vec::new();
        if let (Some(subject), Some(attribute)) = (op.subject.as_deref(), op.attribute.as_deref()) {
            for (idx, entry) in self.entries.iter().enumerate() {
                if entry.is_active(now)
                    && entry.subject.as_deref() == Some(subject)
                    && entry.attribute.as_deref() == Some(attribute)
                {
                    conflicts.push(idx);
                }
            }
        }
        // R3：保留旧式短语冲突检测（偏好/always 等），确保删 nudge 后不丢失。
        conflicts.extend(detect_conflicts(&new_entry, &self.entries));
        conflicts.sort_unstable();
        conflicts.dedup();
        report.conflicts += conflicts.len();
        for idx in conflicts {
            if let Some(entry) = self.entries.get_mut(idx) {
                entry.supersede(content.to_string(), now);
            }
        }

        let id = entry_id(content);
        self.semantic.add_l1_entry(&id, content, &new_entry.source);
        self.entries.push(new_entry);
        true
    }

    fn apply_retract(&mut self, op: &MemoryOp, now: i64) -> bool {
        if let Some(subject) = op.subject.as_deref() {
            let attribute = op.attribute.clone();
            self.retracted_subjects.push(RetractedSubject {
                subject: subject.to_string(),
                attribute: attribute.clone(),
                retracted_at: now,
            });
            let mut removed = 0usize;
            let mut kept = Vec::with_capacity(self.entries.len());
            for mut entry in std::mem::take(&mut self.entries) {
                let hits_attribute = match attribute.as_deref() {
                    Some(attr) => entry.attribute.as_deref() == Some(attr),
                    None => true,
                };
                if entry.is_active(now)
                    && entry.subject.as_deref() == Some(subject)
                    && hits_attribute
                {
                    entry.supersede(format!("retracted:{subject}"), now);
                    self.archive.push(entry);
                    removed += 1;
                } else {
                    kept.push(entry);
                }
            }
            self.entries = kept;
            return removed > 0;
        }
        if let Some(target) = op.target.as_deref() {
            return self.remove_entry(target);
        }
        false
    }

    /// 是否命中撤回集合（R4/A4：subject+attribute 拦截，挡得住重抽）。
    #[must_use]
    pub fn is_retracted(&self, subject: Option<&str>, attribute: Option<&str>) -> bool {
        let Some(subject) = subject else {
            return false;
        };
        self.retracted_subjects.iter().any(|retracted| {
            retracted.subject == subject
                && match (&retracted.attribute, attribute) {
                    (None, _) => true,
                    (Some(a), Some(b)) => a == b,
                    (Some(_), None) => false,
                }
        })
    }

    /// 遗忘硬规则（R4）：180 天未召回/验证降级归档 + active 上限淘汰。
    ///
    /// 返回本次降级/淘汰的条目数。执行点：`apply_ops`、`load_and_freeze`、
    /// `on_memory_boundary`（经 `apply_ops`/显式调用）。
    pub fn enforce_limits(&mut self, now: i64, max_active: usize) -> u64 {
        let mut evicted = 0u64;
        let mut kept = Vec::with_capacity(self.entries.len());
        for mut entry in std::mem::take(&mut self.entries) {
            let last_touch = entry
                .last_recalled_at
                .unwrap_or(entry.last_verified_at)
                .max(entry.last_verified_at)
                .max(entry.valid_from);
            if entry.is_active(now) && now - last_touch > ACTIVE_MAX_AGE_MS {
                entry.supersede("ttl:180d", now);
                self.archive.push(entry);
                evicted += 1;
            } else {
                kept.push(entry);
            }
        }
        self.entries = kept;

        if self.entries.len() > max_active {
            let mut active = std::mem::take(&mut self.entries);
            active.sort_by_key(|entry| {
                entry
                    .last_recalled_at
                    .unwrap_or(entry.last_verified_at)
                    .max(entry.valid_from)
            });
            let drop_count = active.len() - max_active;
            for mut entry in active.drain(0..drop_count) {
                entry.supersede("cap:active", now);
                self.archive.push(entry);
                evicted += 1;
            }
            self.entries = active;
        }
        evicted
    }

    /// 边界巩固（R1）：执行遗忘规则，必要时 consolidate。
    ///
    /// 只做内存操作与落盘，不做 embedding、不调用 LLM；失败静默。
    pub fn sweep(&mut self) -> u64 {
        let now = now_ms();
        let evicted = self.enforce_limits(now, ACTIVE_MAX_ENTRIES);
        if evicted > 0 || self.needs_consolidation() {
            self.consolidate();
        }
        evicted
    }

    /// 撤销最近一次撤回（`/memory undo`）：移除墓碑，允许该事实重新写入。
    pub fn unretract_last(&mut self) -> Option<RetractedSubject> {
        let removed = self.retracted_subjects.pop();
        if removed.is_some() {
            self.persist_or_warn("unretract");
        }
        removed
    }

    /// 撤回集合（`/memory` 审查面展示用）。
    #[must_use]
    pub fn retracted_subjects(&self) -> &[RetractedSubject] {
        &self.retracted_subjects
    }

    /// 审阅用：这条 op 若此刻生效，会被判成"新增 / 去重 / 覆盖 / 拦截"。
    #[must_use]
    pub fn preview_op(&self, op: &MemoryOp) -> String {
        let now = now_ms();
        if self.is_retracted(op.subject.as_deref(), op.attribute.as_deref()) {
            return "拦截（subject 已撤回）".to_string();
        }
        if op.op == OpKind::Retract {
            return match op.subject.as_deref() {
                Some(subject) => format!("撤回（subject={subject}）"),
                None => "撤回（按 target 匹配）".to_string(),
            };
        }
        let content = op.content.trim();
        if let Some(existing) = self
            .entries
            .iter()
            .find(|entry| entry.is_active(now) && entry.content == content)
        {
            return format!("去重（已存在：{}）", existing.content);
        }
        if let (Some(subject), Some(attribute)) = (op.subject.as_deref(), op.attribute.as_deref()) {
            let conflicts = self
                .entries
                .iter()
                .filter(|entry| {
                    entry.is_active(now)
                        && entry.subject.as_deref() == Some(subject)
                        && entry.attribute.as_deref() == Some(attribute)
                })
                .count();
            if conflicts > 0 {
                return format!("覆盖（与 {conflicts} 条同 subject+attribute 条目冲突）");
            }
        }
        "新增".to_string()
    }

    /// Persist the current in-memory state to disk.
    fn persist(&self) -> std::io::Result<()> {
        MemoryStore::new(self.file_path.clone()).save(self)
    }

    /// 边界全量落盘：会话切换 / 进程退出 / serve worker 回收时调用。
    ///
    /// turn 路径上的 `apply_ops` 只改内存；崩溃后由追加日志回放兜底。
    /// 影子条目是 `#[serde(skip)]`，不会被写入。
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.persist()
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
        // 影子条目优先按内容撤销（`/memory rm` 当场否掉影子记忆）。
        if let Some(idx) = self
            .shadow_entries
            .iter()
            .position(|entry| entry.content.contains(pattern))
        {
            self.shadow_entries.remove(idx);
            return true;
        }
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
            // 不再同步落盘：本方法也会被 `apply_ops`（turn 路径）调用，
            // 全量持久化统一交给边界 `flush`，崩溃由 journal 回放兜底。
            return true;
        }
        false
    }
}

// ===== Q5(2026-11-19):逐轮用户风格/偏好观察 =====
//
// 用户诉求:「提取我的每一次发言，从中理解我的喜好和风格，并固化到记忆中」。
//
// 与 Phase 1 LLM 提取器（[`crate::memory_extract`]）的分工：
// * LLM 提取器按**内容**标准抽取（稳定偏好 / 用户纠正 / 踩坑根因 / 长期约束），
//   默认 NOOP，受窗口合并与二现门槛约束 —— 适合稀疏的高价值事实；
// * 本通道按**表达形式**做确定性模式匹配，零 LLM、零延迟、可每轮运行 ——
//   覆盖「风格」这类高频、单条信息量小但累积价值高的信号。
//
// 为何风格通道不用 LLM：
// * Salemi et al., *LaMP: When Large Language Models Meet Personalization*
//   (ACL 2024, arXiv:2304.11406) 显示 profile 的收益来自**相关性**而非规模，
//   检索回来的少量高相关 profile 优于全量堆叠；
// * *The Personalization Mirage* (arXiv:2608.04570, 2026) 指出 LLM 自建
//   profile 会系统性**过度推断**（over-inference）—— 凭空补出证据不支持的
//   用户属性，且自监测（self-monitoring）无法可靠拦截。
// 确定性关键词匹配的召回上限低，但**不会编造**。对长期记忆而言精度优先于
// 召回：一条错误的长期偏好会污染此后每个会话的系统前缀。
//
// 写入路径遵循 Phase 1 的边界落盘契约：构造 [`MemoryOp`] → `apply_ops`
// （纯内存）→ 追加 journal；全量快照由会话边界 `flush` 统一落盘。因此本通道
// 每轮运行也不会产生每轮全量写盘。subject/attribute 取自受控词表
// （`work_style.*` / `language_pref.reply_language`），同一类信号反复出现
// 只会刷新 `last_verified_at`，不会膨胀记忆。

/// 一条风格信号的受控归类。
///
/// `subject` / `attribute` 必须存在于 [`MemoryTaxonomy`] 词表中，且**同一
/// subject 下各信号必须用不同 attribute** —— [`PersistentMemory::apply_upsert`]
/// 按 subject+attribute 覆盖旧条目，共用一个 attribute 会让后写入的信号
/// 逐条顶掉先写入的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyleSignal {
    /// 受控 subject（如 `work_style` / `language_pref`）。
    pub subject: &'static str,
    /// 受控 attribute（如 `answer_length` / `reply_language`）。
    pub attribute: &'static str,
    /// 可直接持久化为记忆条目的短事实。
    pub content: &'static str,
}

/// 从单条用户发言中提取确定性的喜好/风格信号（零 LLM）。
///
/// 只做**保守**判定：宁可漏掉，也不把噪音写进长期记忆。无信号时返回空 vec。
///
/// 覆盖 6 类可观测的表达形式偏好：回复语言、详略、结构化输出、自主性
/// （不要反问）、第一性原理、权威来源要求。
#[must_use]
pub fn extract_style_signals(text: &str) -> Vec<StyleSignal> {
    let mut signals = Vec::new();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return signals;
    }
    let lower = trimmed.to_lowercase();
    let contains_any = |keys: &[&str]| keys.iter().any(|key| lower.contains(key));

    // 1. 回复语言：CJK 存在 ⇒ 用户以中文交流，默认用中文回复。
    if trimmed
        .chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    {
        signals.push(StyleSignal {
            subject: "language_pref",
            attribute: "reply_language",
            content: "user writes in Chinese; reply in Chinese by default",
        });
    }

    // 2. 详略偏好（简洁 vs 详尽）。两者互斥，用同一 attribute 让后者顶掉前者：
    //    用户先要详细、后说别啰嗦，最终生效的应是「简洁」。
    if contains_any(&[
        "简洁",
        "简短",
        "精简",
        "别啰嗦",
        "concise",
        "be brief",
        "keep it short",
    ]) {
        signals.push(StyleSignal {
            subject: "work_style",
            attribute: "answer_length",
            content: "user prefers concise, to-the-point answers",
        });
    }
    if contains_any(&[
        "详细",
        "展开",
        "具体说明",
        "逐条",
        "in detail",
        "elaborate",
        "explain fully",
    ]) {
        signals.push(StyleSignal {
            subject: "work_style",
            attribute: "answer_length",
            content: "user wants thorough, detailed explanations",
        });
    }

    // 3. 结构化输出偏好。
    if contains_any(&["表格", "列表", "清单", "table", "bullet"]) {
        signals.push(StyleSignal {
            subject: "work_style",
            attribute: "output_format",
            content: "user likes structured output (tables/lists)",
        });
    }

    // 4. 自主性：用户明确表示不要被反问。
    if contains_any(&[
        "不要问我",
        "别问我",
        "自行决策",
        "自己决定",
        "don't ask me",
        "decide yourself",
        "no questions",
    ]) {
        signals.push(StyleSignal {
            subject: "work_style",
            attribute: "autonomy",
            content: "user wants autonomous decisions — do not ask, decide and proceed",
        });
    }

    // 5. 严谨性：第一性原理 / 权威来源。两项独立，故用不同 attribute。
    if contains_any(&["第一性原理", "first principle"]) {
        signals.push(StyleSignal {
            subject: "work_style",
            attribute: "reasoning_style",
            content: "user values first-principles reasoning",
        });
    }
    if contains_any(&[
        "论文",
        "权威",
        "引用",
        "paper",
        "citation",
        "cite",
        "reference",
    ]) {
        signals.push(StyleSignal {
            subject: "work_style",
            attribute: "sourcing",
            content: "user expects authoritative sources/citations for claims",
        });
    }

    signals
}

/// 把逐轮风格信号折入记忆，返回**新增**条目数。
///
/// `texts` 通常是最近若干条 user 发言（至少含本轮发言）。幂等：同一信号
/// 重复出现只刷新 `last_verified_at`，不新增条目。
///
/// 调用方无需自己做内容去重 —— [`PersistentMemory::apply_upsert`] 的 L1
/// 精确内容判定已经覆盖；本函数只用 `seen` 消除**同一批内**的重复 op。
pub fn observe_user_style(memory: &mut PersistentMemory, texts: &[String]) -> usize {
    let before = memory.entries.len();
    let mut seen: std::collections::HashSet<(&'static str, &'static str)> =
        std::collections::HashSet::new();
    let mut ops: Vec<MemoryOp> = Vec::new();
    for text in texts {
        // 注入块（`[Web 计划…]` 等外壳）不是用户发言，必须先剥离：
        // 否则「任务/计划」等字样会被当成用户的表达偏好写进长期记忆。
        let cleaned = crate::task_state::strip_injected_prefix(text);
        for signal in extract_style_signals(cleaned) {
            if !seen.insert((signal.subject, signal.attribute)) {
                continue;
            }
            ops.push(MemoryOp {
                op: OpKind::Upsert,
                kind: EntryKind::UserProfile,
                subject: Some(signal.subject.to_string()),
                attribute: Some(signal.attribute.to_string()),
                content: signal.content.to_string(),
                confidence: Confidence::High,
                basis: Basis::Inferred,
                explicit: false,
                evidence_count: 1,
                source: "style-observer".to_string(),
                ..MemoryOp::default()
            });
        }
    }
    if ops.is_empty() {
        return 0;
    }
    let report = memory.apply_ops(&ops, ACTIVE_MAX_ENTRIES);
    if report.applied > 0 {
        crate::memory_extract::append_journal(&memory.file_path, &ops);
    }
    memory.entries.len().saturating_sub(before)
}

/// `memory_update` 工具的 Tool specification(JSON schema)。
///
/// 注册到 LLM 的 tool list,让 LLM 主动管理 PersistentMemory(MemGPT 风格):
/// 更新 Persona/Human/Tasks 块(下个会话进入 system 前缀)或增删改语义
/// 记忆 entries(本会话经语义召回可见)。
pub const MEMORY_UPDATE_TOOL_SPEC: &str = r#"{
    "name": "memory_update",
    "description": "主动管理持久记忆（MemGPT 风格）：更新 Persona/Human/Tasks 块（跨会话进入 system 前缀），或增删改语义记忆 entries（本会话经语义召回可见）。块写入下个会话生效，entries 本会话生效。",
    "input_schema": {
        "type": "object",
        "properties": {
            "block": {"type": "string", "enum": ["persona", "human", "tasks"]},
            "content": {"type": "string"},
            "op": {"type": "string", "enum": ["add_entry", "replace_entry", "remove_entry"]},
            "pattern": {"type": "string"},
            "source": {"type": "string"}
        },
        "additionalProperties": false
    }
}"#;

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

/// 归一化字符二元组 Jaccard 相似度（A6：只用于同 subject 内 >0.95 的跳过判定）。
fn jaccard_similarity(a: &str, b: &str) -> f64 {
    use std::collections::HashSet;
    fn bigrams(text: &str) -> HashSet<String> {
        let chars: Vec<char> = text
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation())
            .flat_map(char::to_lowercase)
            .collect();
        if chars.len() < 2 {
            return chars.into_iter().map(|c| c.to_string()).collect();
        }
        chars.windows(2).map(|pair| pair.iter().collect()).collect()
    }
    let left = bigrams(a);
    let right = bigrams(b);
    if left.is_empty() && right.is_empty() {
        return if a == b { 1.0 } else { 0.0 };
    }
    let intersection = left.intersection(&right).count();
    let union = left.union(&right).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
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

/// Return a compiled regex for detecting English memory keywords with
/// optional conjugation suffixes. Compiled once and cached for the process
/// lifetime.
fn contradiction_regex_en() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(prefer|always|never|use|like|hate)(?:s|ed|d)?\b\s+(.+?)(?:[.,;\n]|$)")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        // 进程内单调序号：保证并发测试的目录**严格唯一**。
        //
        // 2026-11-27 修复：原先只用纳秒时间戳命名，但 Windows 时钟分辨率不足
        // （实测连续 50 次 `as_nanos()` 仅 36 个唯一值），并发测试会撞同一目录，
        // 互相覆盖 memory.json 并回放对方的 memory_pending.jsonl，表现为
        // `hits.len()==0` 而日志显示 "indexed 2 entries"。
        // 违反铁律 I7（测试不得依赖精确计时）。
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        // 每个测试独占一个目录：崩溃回放的 journal 落在 memory.json 同目录
        // (`memory_pending.jsonl`)，共用临时根目录会让测试之间互相回放对方的
        // 待应用 op。
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("claw-memory-{nanos}-{seq}"));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("memory.json")
    }

    // --- P2-7: PersistentMemory + Mid-Session Freeze ----------------------

    // --- Q5: 逐轮风格观察通道 ---------------------------------------------

    #[test]
    fn extract_style_signals_detects_preferences() {
        let zh = extract_style_signals("请你简洁一点，不要问我，自行决策，参考权威论文");
        let contents: Vec<&str> = zh.iter().map(|signal| signal.content).collect();
        assert!(
            contents.contains(&"user writes in Chinese; reply in Chinese by default"),
            "contents: {contents:?}"
        );
        assert!(
            contents.contains(&"user prefers concise, to-the-point answers"),
            "contents: {contents:?}"
        );
        assert!(
            contents.contains(&"user wants autonomous decisions — do not ask, decide and proceed"),
            "contents: {contents:?}"
        );
        assert!(
            contents.contains(&"user expects authoritative sources/citations for claims"),
            "contents: {contents:?}"
        );
    }

    #[test]
    fn extract_style_signals_ignores_noise() {
        assert!(extract_style_signals("ok").is_empty());
        assert!(extract_style_signals("   ").is_empty());
        assert!(extract_style_signals("").is_empty());
    }

    /// 六类信号必须落在**互不相同**的 subject+attribute 上。
    ///
    /// `apply_upsert` 按 subject+attribute 覆盖旧条目：若共用一个 attribute，
    /// 同一批写入的信号会互相顶掉，最终只剩最后一条。这条测试锁住词表与
    /// 信号表的对齐关系，防止后人「顺手」把 attribute 合并。
    #[test]
    fn style_signals_use_distinct_subject_attribute_pairs() {
        let all = extract_style_signals(
            "简洁一点，用表格列出，不要问我，第一性原理，引用论文，回复用中文",
        );
        assert!(all.len() >= 6, "all: {all:?}");
        let mut pairs: Vec<(&str, &str)> = all
            .iter()
            .map(|signal| (signal.subject, signal.attribute))
            .collect();
        let total = pairs.len();
        pairs.sort_unstable();
        pairs.dedup();
        assert_eq!(
            pairs.len(),
            total,
            "同一批信号不得共用 subject+attribute：{pairs:?}"
        );
    }

    /// 详略偏好是互斥的：共用 `answer_length` 让后写的顶掉先写的。
    #[test]
    fn concise_and_detailed_share_answer_length_attribute() {
        // 中文发言会同时命中「回复语言」信号，故按 attribute 过滤后再断言。
        let pick = |text: &str| -> Vec<(&'static str, &'static str)> {
            extract_style_signals(text)
                .into_iter()
                .filter(|signal| signal.attribute == "answer_length")
                .map(|signal| (signal.subject, signal.content))
                .collect()
        };
        let concise = pick("简洁一点");
        let detailed = pick("请详细展开");
        assert_eq!(concise.len(), 1, "concise: {concise:?}");
        assert_eq!(detailed.len(), 1, "detailed: {detailed:?}");
        assert_eq!(concise[0].0, detailed[0].0, "subject 应相同");
        assert_ne!(concise[0].1, detailed[0].1, "内容应不同");
    }

    #[test]
    fn observe_user_style_is_idempotent() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        let texts = vec!["请你简洁一点，不要问我，自行决策".to_string()];
        let first = observe_user_style(&mut mem, &texts);
        assert!(first > 0, "首次观察应写入条目");
        let after_first = mem.entries().len();
        let again = observe_user_style(&mut mem, &texts);
        assert_eq!(again, 0, "重复观察不得新增条目");
        assert_eq!(mem.entries().len(), after_first, "条目数不得膨胀");
        assert!(
            mem.entries()
                .iter()
                .any(|entry| entry.source == "style-observer"),
            "条目来源应为 style-observer"
        );
    }

    /// 注入块（`[Web 计划…]`）不是用户发言，不得被当成表达偏好写进记忆。
    #[test]
    fn observe_user_style_strips_injected_prefix() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        let injected = "[Web 计划（用户在 Web 端维护，作为当前执行计划）]\n\
             任务: 简洁一点\n\n真实发言: 不要问我"
            .to_string();
        observe_user_style(&mut mem, &[injected]);
        // 整块被剥离后只剩「真实发言: 不要问我」→ 只应命中自主性一条。
        let contents: Vec<&str> = mem
            .entries()
            .iter()
            .map(|entry| entry.content.as_str())
            .collect();
        assert!(
            contents.contains(&"user wants autonomous decisions — do not ask, decide and proceed"),
            "contents: {contents:?}"
        );
        assert!(
            !contents.contains(&"user prefers concise, to-the-point answers"),
            "注入块内的「简洁」不应被当作用户偏好：{contents:?}"
        );
    }

    /// 风格条目走 `UserProfile` 类别 ⇒ 进入 Human 只读投影（下个会话前缀）。
    #[test]
    fn observe_user_style_marks_entries_as_user_profile() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        observe_user_style(&mut mem, &["回复用中文".to_string()]);
        let entry = mem
            .entries()
            .iter()
            .find(|entry| entry.source == "style-observer")
            .expect("应有 style-observer 条目");
        assert_eq!(entry.kind, EntryKind::UserProfile);
        assert_eq!(entry.subject.as_deref(), Some("language_pref"));
        assert_eq!(entry.attribute.as_deref(), Some("reply_language"));
        assert!(!entry.orphan, "受控词表内的条目不得标孤儿");
    }

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
        assert_eq!(
            before, after,
            "frozen snapshot must stay stable within session"
        );
        assert!(!after.contains("user lives in Berlin"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mid_session_entry_recallable_in_current_session_prefix_stable() {
        // C 修复回归(2026-08-30):会话中 nudge/写入的 entry 立即可经语义召回
        // 通道在本会话命中(add_entry 同步更新 L1 索引),而非"下个会话才生效";
        // 同时冻结前缀字节保持稳定(召回走尾部动态槽位,不动前缀)。
        // 用 empty() 构造避免测试环境注入全局 embedding provider,保证走
        // 确定性 keyword 匹配,消除环境依赖 flaky。
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.add_entry("user prefers dark mode terminals", "test-session");
        let hits = mem.semantic_recall("dark mode", 3);
        assert!(
            hits.iter().any(|h| h.entry.summary.contains("dark mode")),
            "new entry must be recallable in the current session: {hits:?}"
        );
        let before = mem.frozen_render();
        mem.add_entry("second fact: tabs preferred", "test-session");
        assert_eq!(
            before,
            mem.frozen_render(),
            "frozen prefix must stay byte-stable after mid-session add_entry"
        );
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
    fn has_content_reflects_blocks_and_active_entries() {
        // P1-5a:全空(三块空 + 无活跃条目)判定无内容 → 注入端省略整段。
        let path = temp_path();
        let mem = PersistentMemory::empty(&path);
        assert!(!mem.has_content(), "全空时应判定无内容");

        // 类型块有内容 → 有内容。
        let mut mem = PersistentMemory::empty(&path);
        mem.update_block("Persona", "沟通语言:中文")
            .expect("update block");
        assert!(mem.has_content(), "块有内容后应有内容");

        // 仅活跃条目 → 也有内容。
        let mut mem2 = PersistentMemory::empty(&path);
        mem2.add_entry("临时事实条目", "test-session");
        assert!(mem2.has_content(), "活跃条目也计入内容判定");

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
            mem.archive()
                .iter()
                .any(|e| e.content == "prefer dark mode"),
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
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.blocks_mut()[0].replace_content("persona".to_string());
        mem.blocks_mut()[1].replace_content("human".to_string());
        mem.blocks_mut()[2].replace_content("tasks".to_string());
        let rendered = mem.render_blocks();
        let persona_pos = rendered.find("## Persona").expect("Persona label");
        let human_pos = rendered.find("## Human").expect("Human label");
        let tasks_pos = rendered.find("## Tasks").expect("Tasks label");
        assert!(persona_pos < human_pos);
        assert!(human_pos < tasks_pos);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_blocks_are_not_rendered() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.blocks_mut()[1].replace_content("human".to_string());
        let rendered = mem.render_blocks();
        assert!(rendered.contains("## Human\nhuman"));
        assert!(!rendered.contains("## Persona"));
        assert!(!rendered.contains("## Tasks"));
        assert!(!rendered.contains("_(empty)_"));
        let _ = std::fs::remove_file(&path);
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
        let (kw, val) =
            extract_keyword_value("用户偏好深色模式").expect("Chinese preference should extract");
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
    fn add_entry_incrementally_embeds_vector_when_provider_injected() {
        // 语义记忆接线回归:注入 HashEmbeddingProvider 后,add_entry 必须
        // 立即计算向量(vectors 非空),使 semantic_recall 走向量路径而非
        // keyword 退化。此前 add_entry 只写 L1 索引不写 vectors,
        // 向量索引恒空,向量召回永不触发。
        use crate::memory_semantic::HashEmbeddingProvider;
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        let provider: std::sync::Arc<dyn crate::memory_semantic::EmbeddingProvider + Send + Sync> =
            std::sync::Arc::new(HashEmbeddingProvider::default_dim());
        mem.semantic = std::mem::take(&mut mem.semantic).with_embedding_provider(provider.clone());
        mem.add_entry("user prefers rust for systems programming", "test");
        // add_entry 后 vectors 应立即包含该 entry 的向量,向量召回路径可用。
        assert_eq!(
            mem.semantic().vectors_count(),
            1,
            "add_entry should embed incrementally"
        );
        // 注入 provider 后 semantic_recall 走向量路径(HashEmbeddingProvider
        // 余弦相似度低于 EMBEDDING_MIN_SCORE=0.85 会被过滤,因此不要求命中;
        // 这里只验证向量路径已接线且不 panic)。
        let hits = mem.semantic_recall("rust systems programming", 5);
        let _ = hits;
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
    fn update_block_updates_persona_human_tasks() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.update_block("persona", "资深 Rust 工程师,偏好简洁架构")
            .expect("persona update");
        mem.update_block("human", "用户使用 Windows,时区 Asia/Shanghai")
            .expect("human update");
        mem.update_block("tasks", "进行中:P2 memory_update 工具")
            .expect("tasks update");
        assert!(mem.blocks()[0].content().contains("Rust"));
        assert!(mem.blocks()[1].content().contains("Windows"));
        assert!(mem.blocks()[2].content().contains("P2 memory_update"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_block_label_is_case_insensitive() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        mem.update_block("PERSONA", "lowercase match")
            .expect("PERSONA");
        mem.update_block("Human", "mixed case").expect("Human");
        mem.update_block("  tasks  ", "trimmed")
            .expect("tasks trimmed");
        assert_eq!(mem.blocks()[0].content(), "lowercase match");
        assert_eq!(mem.blocks()[1].content(), "mixed case");
        assert_eq!(mem.blocks()[2].content(), "trimmed");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_block_unknown_label_errors() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        let err = mem
            .update_block("soul", "no such block")
            .expect_err("must error");
        assert!(err.contains("unknown memory block 'soul'"), "err: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_block_truncates_over_budget_with_ellipsis() {
        let path = temp_path();
        let mut mem = PersistentMemory::empty(&path);
        // Persona block budget is 500 chars; feed 600.
        let long = "字".repeat(600);
        mem.update_block("persona", &long).expect("update");
        let content = mem.blocks()[0].content();
        assert_eq!(content.chars().count(), 501, "500 chars + trailing …");
        assert!(content.ends_with('…'), "truncated content must end with …");
        // Human block budget is 1000; feed exactly 1000 → no truncation.
        mem.update_block("human", &"a".repeat(1000))
            .expect("update");
        assert_eq!(mem.blocks()[1].content().chars().count(), 1000);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_block_persists_and_survives_reload() {
        let path = temp_path();
        {
            let mut mem = PersistentMemory::empty(&path);
            mem.update_block("persona", "跨会话持久化的 persona")
                .expect("update");
            mem.update_block("tasks", "归档任务清单").expect("update");
        }
        let reloaded = PersistentMemory::load_and_freeze(&path);
        let frozen = reloaded.frozen_render();
        assert!(
            frozen.contains("跨会话持久化的 persona"),
            "reloaded frozen render must contain updated persona block: {frozen}"
        );
        assert!(
            frozen.contains("归档任务清单"),
            "tasks block missing: {frozen}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn memory_update_tool_spec_is_valid_json() {
        let spec: serde_json::Value = serde_json::from_str(MEMORY_UPDATE_TOOL_SPEC)
            .expect("MEMORY_UPDATE_TOOL_SPEC must be valid JSON");
        assert_eq!(spec["name"], "memory_update");
        assert_eq!(
            spec["input_schema"]["properties"]["block"]["enum"][0],
            "persona"
        );
        assert_eq!(
            spec["input_schema"]["properties"]["op"]["enum"][2],
            "remove_entry"
        );
    }
}

// ----- Phase 1 closed-loop migration tests (R3/R4/R5) -----
#[cfg(test)]
mod phase1_tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("claw-memory-phase1-{tag}-{nanos}.json"))
    }

    fn upsert(content: &str) -> MemoryOp {
        MemoryOp {
            op: OpKind::Upsert,
            content: content.to_string(),
            source: "test".to_string(),
            basis: Basis::UserStated,
            ..MemoryOp::default()
        }
    }

    #[test]
    fn upsert_same_subject_attribute_supersedes_old() {
        let path = temp_path("upsert");
        let mut memory = PersistentMemory::empty(&path);
        let mut op = upsert("缩进用 tabs");
        op.subject = Some("editor".to_string());
        op.attribute = Some("indent".to_string());
        let first = memory.apply_ops(std::slice::from_ref(&op), ACTIVE_MAX_ENTRIES);
        assert_eq!(first.applied, 1);

        let mut second = upsert("缩进用 spaces");
        second.subject = Some("editor".to_string());
        second.attribute = Some("indent".to_string());
        let report = memory.apply_ops(std::slice::from_ref(&second), ACTIVE_MAX_ENTRIES);
        assert_eq!(report.applied, 1);
        assert!(
            report.conflicts >= 1,
            "should supersede the conflicting entry"
        );

        let now = now_ms();
        let active: Vec<&MemoryEntry> = memory
            .entries()
            .iter()
            .filter(|entry| entry.is_active(now))
            .collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].content, "缩进用 spaces");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn free_text_subjects_collapse_and_old_value_is_auditable() {
        let path = temp_path("taxonomy-collapse");
        let mut memory = PersistentMemory::empty(&path);
        let cases = [
            ("主数据库", "主数据库", "我们用 PostgreSQL 16 作为主数据库"),
            ("项目数据库", "用哪个", "主数据库是 PostgreSQL 16"),
            ("项目技术栈", "技术栈选了", "技术栈里的数据库是 MySQL 8.0"),
            ("project", "主数据库", "主数据库换成 MySQL 8.0"),
            ("数据库", "primary", "最终主数据库是 PostgreSQL 16"),
        ];
        for (subject, attribute, content) in cases {
            let mut op = upsert(content);
            op.subject = Some(subject.to_string());
            op.attribute = Some(attribute.to_string());
            memory.apply_ops(std::slice::from_ref(&op), ACTIVE_MAX_ENTRIES);
        }

        let active = memory.active_entries(now_ms());
        assert_eq!(active.len(), 1, "同一事实漂移后只能剩 1 条活跃: {active:?}");
        assert!(active[0].content.contains("PostgreSQL"));
        assert_eq!(active[0].subject.as_deref(), Some("database"));
        assert_eq!(active[0].attribute.as_deref(), Some("primary"));

        // MySQL 旧值必须仍可查（supersede，不是删除）。
        let mysql = memory
            .entries()
            .iter()
            .chain(memory.archive().iter())
            .find(|entry| entry.content.contains("MySQL"))
            .expect("MySQL 旧值应保留为失效记录");
        assert!(mysql.superseded_by.is_some());
        assert!(mysql.valid_until.is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_free_text_subject_migrates_or_is_flagged_orphan() {
        let path = temp_path("taxonomy-migrate");
        let mut memory = PersistentMemory::empty(&path);
        memory.add_entry("主数据库是 PostgreSQL", "test");
        if let Some(entry) = memory.entries.first_mut() {
            entry.subject = Some("项目数据库".to_string());
            entry.attribute = Some("主数据库".to_string());
        }
        memory.add_entry("某个未归类事实", "test");
        if let Some(entry) = memory.entries.last_mut() {
            entry.subject = Some("完全未知类目".to_string());
        }
        memory.flush().expect("persist baseline");

        let reloaded = PersistentMemory::load_and_freeze(&path);
        let mapped = reloaded
            .entries()
            .iter()
            .find(|entry| entry.content.contains("PostgreSQL"))
            .expect("mapped entry");
        assert_eq!(mapped.subject.as_deref(), Some("database"));
        assert_eq!(mapped.attribute.as_deref(), Some("primary"));
        assert!(!mapped.orphan);

        let orphan = reloaded
            .entries()
            .iter()
            .find(|entry| entry.content.contains("未归类"))
            .expect("orphan entry");
        assert!(orphan.orphan);
        assert_eq!(orphan.legacy_subject.as_deref(), Some("完全未知类目"));
        assert_eq!(orphan.subject.as_deref(), Some("other"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn orphan_entries_are_neither_rendered_nor_recalled() {
        // 孤儿条目（subject 无法映射到受控词表）的「归哪一类」本身未定，
        // 因此既不得进入注入前缀，也不得进入语义召回索引。
        //
        // 实测污染：`.claw/memory.json` 里 6 条 block-migration 孤儿每轮都
        // 以 `## 跨任务通用教训…` 的形式出现在 system 前缀，且
        // `semantic_recall` 持续返回其中一条（score 0.862）——它们既无法
        // 被 subject+attribute 覆盖语义管理，也不该占用缓存前缀预算。
        let path = temp_path("orphan-not-rendered");
        let mut memory = PersistentMemory::empty(&path);
        memory.add_entry("主数据库是 PostgreSQL", "test");
        if let Some(entry) = memory.entries.first_mut() {
            entry.subject = Some("项目数据库".to_string());
            entry.attribute = Some("主数据库".to_string());
        }
        memory.add_entry("某个未归类事实", "test");
        if let Some(entry) = memory.entries.last_mut() {
            entry.subject = Some("完全未知类目".to_string());
        }
        memory.flush().expect("persist baseline");

        let reloaded = PersistentMemory::load_and_freeze(&path);
        let orphan = reloaded
            .entries()
            .iter()
            .find(|entry| entry.content.contains("未归类"))
            .expect("orphan entry");
        assert!(orphan.orphan, "前置条件：该条目应被标为孤儿");

        let rendered = reloaded.frozen_render();
        assert!(
            !rendered.contains("未归类"),
            "孤儿条目不得进入注入前缀: {rendered}"
        );
        assert!(
            rendered.contains("PostgreSQL"),
            "受控词表内的条目仍应正常渲染: {rendered}"
        );
        assert!(
            !reloaded
                .semantic_recall("未归类事实", 5)
                .iter()
                .any(|hit| hit.entry.summary.contains("未归类")),
            "孤儿条目不得进入语义召回索引"
        );
        // 仍保留在 entries 里供 `/memory` 面板审阅与人工归位。
        assert_eq!(
            reloaded.orphan_entries().len(),
            1,
            "孤儿应仍可被 orphan_entries() 列出以便人工归位"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn retract_subject_blocks_reinsert() {
        let path = temp_path("retract");
        let mut memory = PersistentMemory::empty(&path);
        let mut op = upsert("不要动 vendor 目录");
        op.subject = Some("project".to_string());
        op.attribute = Some("vendor_dir".to_string());
        assert_eq!(
            memory
                .apply_ops(std::slice::from_ref(&op), ACTIVE_MAX_ENTRIES)
                .applied,
            1
        );

        let retract = MemoryOp {
            op: OpKind::Retract,
            subject: Some("project".to_string()),
            attribute: Some("vendor_dir".to_string()),
            ..MemoryOp::default()
        };
        let report = memory.apply_ops(std::slice::from_ref(&retract), ACTIVE_MAX_ENTRIES);
        assert_eq!(report.retracted, 1);
        assert!(memory.is_retracted(Some("project"), Some("vendor_dir")));

        let again = memory.apply_ops(std::slice::from_ref(&op), ACTIVE_MAX_ENTRIES);
        assert_eq!(again.applied, 0);
        assert_eq!(again.skipped, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ttl_downgrades_entries_older_than_180_days() {
        let path = temp_path("ttl");
        let mut memory = PersistentMemory::empty(&path);
        memory.add_entry("old fact", "test");
        let now = now_ms();
        if let Some(entry) = memory.entries.first_mut() {
            entry.valid_from = now - ACTIVE_MAX_AGE_MS - 1_000;
            entry.last_verified_at = now - ACTIVE_MAX_AGE_MS - 1_000;
            entry.last_recalled_at = None;
        }
        let evicted = memory.enforce_limits(now, ACTIVE_MAX_ENTRIES);
        assert_eq!(evicted, 1);
        assert!(memory.entries.is_empty());
        assert_eq!(memory.archive.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn active_cap_evicts_oldest_entries() {
        let path = temp_path("cap");
        let mut memory = PersistentMemory::empty(&path);
        for index in 0..5 {
            memory.add_entry(&format!("fact {index}"), "test");
        }
        let evicted = memory.enforce_limits(now_ms(), 3);
        assert_eq!(evicted, 2);
        assert_eq!(memory.entries.len(), 3);
        assert_eq!(memory.archive.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn blocks_migrate_once_and_then_freeze() {
        let path = temp_path("blocks");
        let mut memory = PersistentMemory::empty(&path);
        memory.update_block("human", "喜欢深色模式").expect("human");
        assert!(memory.blocks()[1].content().contains("深色"));

        assert!(memory.migrate_blocks_once());
        assert!(memory.blocks()[1].content().is_empty());
        assert!(memory.render_projection().contains("喜欢深色模式"));

        // 迁移后 block 写入冻结：写入 entries，不再改块。
        memory
            .update_block("human", "偏好浅色")
            .expect("frozen write");
        assert!(memory.blocks()[1].content().is_empty());
        assert!(
            memory
                .entries()
                .iter()
                .any(|entry| entry.content == "偏好浅色"),
            "frozen block writes must land in entries"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn human_projection_and_entries_do_not_duplicate() {
        let path = temp_path("projection");
        let mut memory = PersistentMemory::empty(&path);
        let mut profile = upsert("用户是后端工程师");
        profile.kind = EntryKind::UserProfile;
        let fact = upsert("项目使用 Rust");
        memory.apply_ops(&[profile, fact], ACTIVE_MAX_ENTRIES);

        let rendered = memory.render_current();
        assert_eq!(
            rendered.matches("用户是后端工程师").count(),
            1,
            "projected entry must not be rendered twice: {rendered}"
        );
        assert!(rendered.contains("## Human"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_phrase_conflict_detection_still_runs_after_nudge_removal() {
        let path = temp_path("conflict");
        let mut memory = PersistentMemory::empty(&path);
        memory.apply_ops(&[upsert("I prefer tabs")], ACTIVE_MAX_ENTRIES);
        memory.apply_ops(&[upsert("I prefer spaces")], ACTIVE_MAX_ENTRIES);
        let now = now_ms();
        let active = memory
            .entries()
            .iter()
            .filter(|entry| entry.is_active(now))
            .count();
        assert_eq!(
            active, 1,
            "contradicting phrase must supersede the old entry"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn consolidate_moves_superseded_entries_to_archive() {
        let path = temp_path("consolidate");
        let mut memory = PersistentMemory::empty(&path);
        let mut first = upsert("old value");
        first.subject = Some("k".to_string());
        first.attribute = Some("v".to_string());
        let mut second = upsert("new value");
        second.subject = Some("k".to_string());
        second.attribute = Some("v".to_string());
        memory.apply_ops(&[first, second], ACTIVE_MAX_ENTRIES);
        memory.consolidate();
        assert!(
            memory
                .archive
                .iter()
                .any(|entry| entry.content == "old value"),
            "superseded entry should be archived by consolidate"
        );
        let _ = std::fs::remove_file(&path);
    }
}
