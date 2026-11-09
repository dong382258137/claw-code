//! LLM-driven memory extraction (Phase 1 of the memory closed-loop fix).
//!
//! 设计依据：`.trae/documents/记忆闭环修复方案.md` §3/§4。
//!
//! 本模块只负责**产出 op**，不直接改 `PersistentMemory`：
//!
//! ```text
//! turn 尾  → enqueue_evidence (O(1) try_send, 绝不等待)
//! worker   → 窗口合并(8 turn / 90s / 边界) → LLM 严格 JSON → 二现门槛 → pending 队列
//! turn 头  → 主线程 take_ops() 纯内存 apply（见 memory::PersistentMemory::apply_ops）
//! ```
//!
//! 约束：
//! * 不在 turn 路径上做任何 I/O、LLM 调用或 embedding；
//! * 提取失败（未注册 client / 解析失败 / 网络错误）一律静默丢弃并计数；
//! * 写入分三档（`CLAW_MEMORY_MODE`）：
//!   - `log-only`（默认）：只记 `.claw/memory_shadow.jsonl` 提议，供 `/memory review` 审阅；
//!   - `visible`：op 进内存投影（可召回），但标 `shadow`、不落 `memory.json`；
//!   - `write`：真实写入 entries（边界时全量落盘 + 追加日志回放）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 窗口合并的 turn 上限：攒够 8 个 turn 的 evidence 就调用一次提取器。
pub const WINDOW_TURNS: usize = 8;
/// 窗口空闲阈值：90s 内没有新 evidence 即冲刷。
pub const WINDOW_IDLE_MS: u64 = 90_000;
/// 单条记忆内容上限（字符）。
pub const MAX_CONTENT_CHARS: usize = 200;
/// 单轮 user 证据上限（字符）。
pub const EVIDENCE_USER_MAX_CHARS: usize = 2048;
/// 单轮 assistant 证据上限（字符）。
pub const EVIDENCE_ASSISTANT_MAX_CHARS: usize = 2048;
/// 单轮工具失败签名条数上限。
pub const EVIDENCE_TOOL_FAILURES_MAX: usize = 5;
/// 单条工具失败签名上限（字符）。
pub const EVIDENCE_TOOL_FAILURE_MAX_CHARS: usize = 100;
/// 候选池上限。
pub const CANDIDATE_MAX_ENTRIES: usize = 50;
/// 候选池过期时间：30 天。
pub const CANDIDATE_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// pending 日志上限（字节），超过后只保留尾部。
pub const PENDING_LOG_MAX_BYTES: u64 = 64 * 1024;
/// worker 命令队列上限。
const WORKER_QUEUE_CAP: usize = 32;

/// 记忆条目类别（A6：五值压到三值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// 用户画像：稳定的自述偏好 / 环境 / 身份。
    UserProfile,
    /// 可复用的踩坑教训。
    Lesson,
    /// 一般事实。
    #[default]
    Fact,
}

/// 操作类型（A6：`update` 与 `supersede` 合并为 `upsert`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// 新增或按 subject+attribute 覆盖。
    #[default]
    Upsert,
    /// 撤销（用户明确否定 / `/memory rm`）。
    Retract,
}

/// 事实来源（A2：二现门槛按来源分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// 用户第一人称自述：一次即落盘。
    UserStated,
    /// 从行为推断：需二现才落盘。
    #[default]
    Inferred,
    /// 约束类：一次即落盘，且必须给可否定回执。
    Constraint,
}

/// 置信度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    #[default]
    Medium,
    Low,
}

/// 一条由提取器产出的记忆操作。
///
/// 所有字段都带 `serde(default)`，解析宽松；缺失 `basis` 时保守回退
/// [`Basis::Inferred`]（宁可漏写，不写脑补）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemoryOp {
    #[serde(default)]
    pub op: OpKind,
    #[serde(default)]
    pub kind: EntryKind,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub confidence: Confidence,
    #[serde(default)]
    pub source: String,
    /// `Retract` 时指向现有 entry id 或 content 片段。
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub evidence_count: u32,
    #[serde(default)]
    pub basis: Basis,
    /// 用户显式祈使（记住/以后都/不要再/记错了…）；由提取器判定。
    #[serde(default)]
    pub explicit: bool,
    /// 影子条目：`visible` 模式下为真，只进内存投影，不参与持久化。
    #[serde(default)]
    pub shadow: bool,
}

impl MemoryOp {
    /// 归一化并做安全过滤；返回 `None` 表示该 op 必须整条丢弃。
    ///
    /// 规则：内容非空、去首尾空白、拒绝疑似密钥/隐私原文、截断到
    /// [`MAX_CONTENT_CHARS`]。
    #[must_use]
    pub fn sanitize(mut self) -> Option<Self> {
        self.content = self.content.trim().to_string();
        if self.content.is_empty() && self.op == OpKind::Upsert {
            return None;
        }
        if looks_like_secret(&self.content) {
            return None;
        }
        if self.content.chars().count() > MAX_CONTENT_CHARS {
            self.content = self.content.chars().take(MAX_CONTENT_CHARS).collect();
        }
        self.subject = self.subject.map(|s| s.trim().to_string());
        self.attribute = self.attribute.map(|s| s.trim().to_string());
        if self.source.trim().is_empty() {
            self.source = "extractor".to_string();
        }
        Some(self)
    }

    /// 去重键：subject|attribute|归一化内容。
    #[must_use]
    pub fn dedup_key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.subject.as_deref().unwrap_or(""),
            self.attribute.as_deref().unwrap_or(""),
            normalize_content(&self.content)
        )
    }
}

/// 一轮对话的可提取证据（已裁剪，禁止塞工具输出原文）。
#[derive(Debug, Clone, Default)]
pub struct TurnEvidence {
    pub session_id: String,
    pub turn_index: u64,
    pub user_text: String,
    pub assistant_text: String,
    pub tool_failures: Vec<String>,
}

impl TurnEvidence {
    /// 构造时统一裁剪，避免把超长上下文带进 worker。
    #[must_use]
    pub fn build(
        session_id: &str,
        turn_index: u64,
        user_text: &str,
        assistant_text: &str,
        tool_failures: Vec<String>,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            turn_index,
            user_text: truncate_chars(user_text.trim(), EVIDENCE_USER_MAX_CHARS),
            assistant_text: truncate_chars(assistant_text.trim(), EVIDENCE_ASSISTANT_MAX_CHARS),
            tool_failures: tool_failures
                .into_iter()
                .take(EVIDENCE_TOOL_FAILURES_MAX)
                .map(|f| truncate_chars(&f, EVIDENCE_TOOL_FAILURE_MAX_CHARS))
                .collect(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.user_text.is_empty() && self.assistant_text.is_empty() && self.tool_failures.is_empty()
    }
}

/// 会话边界信号（R1）。由 `/clear`、`/resume`、进程退出、空闲兜底触发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySessionBoundary {
    /// 会话切换（`/clear` / `/resume` / 新建会话）。
    Switched,
    /// 会话关闭（退出到会话列表 / worker 回收）。
    Closed,
    /// 进程正常退出。
    Exited,
    /// 空闲兜底。
    Idle,
}

/// 提取器 client —— 由 CLI 层注入，内部走廉价模型（flash）。
///
/// 与 `planner::PlanGeneratorClient` 同构：同步 trait，失败返回 `Err`，
/// runtime 侧一律静默丢弃。
pub trait MemoryExtractorClient: Send + Sync {
    /// 输入完整 prompt，返回模型原始文本（应为 `{"ops":[...]}`）。
    fn extract(&self, prompt: &str) -> Result<String, String>;
}

static GLOBAL_EXTRACTOR: OnceLock<Option<Arc<dyn MemoryExtractorClient>>> = OnceLock::new();

/// 注册全局提取器 client（进程级单例，重复注册静默忽略）。
pub fn set_global_memory_extractor_client(client: Arc<dyn MemoryExtractorClient>) {
    let _ = GLOBAL_EXTRACTOR.set(Some(client));
}

/// 是否已注册提取器（诊断 / 测试用）。
#[must_use]
pub fn is_memory_extractor_registered() -> bool {
    GLOBAL_EXTRACTOR.get().is_some_and(|slot| slot.is_some())
}

/// 记忆写入档位（`CLAW_MEMORY_MODE`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryMode {
    /// 默认：只记录提议，供审阅，不进 entries。
    #[default]
    LogOnly,
    /// 进内存投影（可召回），标 shadow，不落持久化。
    Visible,
    /// 真实写入 entries。
    Write,
}

/// 当前写入档位：默认 `log-only`，非法值同样回退 `log-only`。
#[must_use]
pub fn memory_mode() -> MemoryMode {
    match std::env::var("CLAW_MEMORY_MODE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "write" => MemoryMode::Write,
        "visible" => MemoryMode::Visible,
        _ => MemoryMode::LogOnly,
    }
}

/// 一条影子提议：记录"当时若生效会记下什么"，供 `/memory review` 按轮审阅。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowProposal {
    pub ts_ms: i64,
    pub session_id: String,
    pub from_turn: u64,
    pub to_turn: u64,
    pub op: MemoryOp,
}

/// 追加写影子提议（失败静默）。
fn append_shadow_proposals(path: &Path, proposals: &[ShadowProposal]) {
    use std::io::Write;
    if proposals.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    for proposal in proposals {
        if let Ok(line) = serde_json::to_string(proposal) {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// 读取影子提议；坏行跳过。
#[must_use]
pub fn load_shadow_proposals(path: &Path) -> Vec<ShadowProposal> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<ShadowProposal>(line.trim()).ok())
        .collect()
}

enum WorkerMsg {
    Evidence(TurnEvidence),
    Boundary(MemorySessionBoundary),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CandidateEntry {
    count: u32,
    first_seen_ms: i64,
    last_seen_ms: i64,
    kind: EntryKind,
    content: String,
    confidence: Confidence,
    source: String,
}

/// 进程级 worker 表：同一 `memory.json` 路径共用同一个 worker。
///
/// 关键原因：CLI 每 turn 都会重建 `ConversationRuntime`（`prepare_turn_runtime`），
/// worker 必须跨 runtime 存活，否则每个 turn 都会新建线程并丢失队列。
static GLOBAL_WORKERS: OnceLock<Mutex<HashMap<PathBuf, Arc<MemoryWorker>>>> = OnceLock::new();

/// 取得（或首次创建）该记忆文件对应的进程级 worker。
pub fn global_worker(memory_path: &Path) -> Arc<MemoryWorker> {
    let map = GLOBAL_WORKERS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = memory_path.to_path_buf();
    if let Ok(mut guard) = map.lock() {
        if let Some(existing) = guard.get(&key) {
            return Arc::clone(existing);
        }
        let worker = Arc::new(MemoryWorker::spawn(&key));
        guard.insert(key, Arc::clone(&worker));
        return worker;
    }
    Arc::new(MemoryWorker::spawn(memory_path))
}

/// 待写入语义索引的预计算向量：`(entry_id, embedding)`。
pub type PendingVector = (String, Vec<f32>);

/// 追加日志（op journal）路径：与 `memory.json` 同目录。
///
/// 该文件同时承担两个职责：崩溃后回放（启动时 apply 到内存），以及
/// 边界全量落盘成功后的清空点。
#[must_use]
pub fn journal_path_for(memory_path: &Path) -> PathBuf {
    memory_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".claw"))
        .join("memory_pending.jsonl")
}

/// 进程内单线程提取 worker。`Drop` 只置 shutdown 标志，不 join（R1：等待上限 0）。
pub struct MemoryWorker {
    tx: SyncSender<WorkerMsg>,
    ops: Arc<Mutex<Vec<MemoryOp>>>,
    vectors: Arc<Mutex<Vec<PendingVector>>>,
    pending_path: PathBuf,
    shadow_path: PathBuf,
    dropped: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
}

impl MemoryWorker {
    /// 基于 `memory.json` 路径启动 worker（状态文件落在同目录 `.claw/`）。
    #[must_use]
    pub fn spawn(memory_path: &Path) -> Self {
        let state_dir = memory_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".claw"));
        let pending_path = journal_path_for(memory_path);
        let shadow_path = state_dir.join("memory_shadow.jsonl");
        let (tx, rx) = sync_channel(WORKER_QUEUE_CAP);
        let ops = Arc::new(Mutex::new(Vec::new()));
        let vectors = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handles = WorkerHandles {
            ops: Arc::clone(&ops),
            vectors: Arc::clone(&vectors),
            dropped: Arc::clone(&dropped),
            shutdown: Arc::clone(&shutdown),
            state_dir: state_dir.clone(),
            pending_path: pending_path.clone(),
            shadow_path: shadow_path.clone(),
        };
        let _ = std::thread::Builder::new()
            .name("claw-memory-extractor".to_string())
            .spawn(move || worker_loop(rx, handles));

        Self {
            tx,
            ops,
            vectors,
            pending_path,
            shadow_path,
            dropped,
            shutdown,
        }
    }

    /// turn 尾入队；队列满即丢并计数，绝不阻塞。
    pub fn enqueue_evidence(&self, evidence: TurnEvidence) {
        if evidence.is_empty() {
            return;
        }
        if self.tx.try_send(WorkerMsg::Evidence(evidence)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 会话边界通知（不等待处理完成）。
    pub fn notify_boundary(&self, boundary: MemorySessionBoundary) {
        let _ = self.tx.try_send(WorkerMsg::Boundary(boundary));
    }

    /// 取走已产出的 op（主线程 drain 用，纯内存）。
    ///
    /// 崩溃回放不在这里做：`PersistentMemory::load_and_freeze` 会在启动时
    /// 回放同一个 journal，再由边界 persist 后清空。
    pub fn take_ops(&self) -> Vec<MemoryOp> {
        self.ops
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    /// 取走 worker 预计算好的向量（主线程只做插入，不做 embedding）。
    pub fn take_vectors(&self) -> Vec<(String, Vec<f32>)> {
        self.vectors
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    #[must_use]
    pub fn pending_log_path(&self) -> &Path {
        &self.pending_path
    }

    #[must_use]
    pub fn shadow_log_path(&self) -> &Path {
        &self.shadow_path
    }

    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 启动时回放 pending 日志（供诊断 / 测试；常规 drain 已自动合并）。
    pub fn take_replayed_ops(&self) -> Vec<MemoryOp> {
        load_pending_ops(&self.pending_path)
    }

    /// 优雅边界后清空 pending 日志（失败静默）。
    pub fn clear_pending_log(&self) {
        let _ = std::fs::remove_file(&self.pending_path);
    }

    /// 主动停止（仅置标志，不 join）。
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for MemoryWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

struct WorkerHandles {
    ops: Arc<Mutex<Vec<MemoryOp>>>,
    vectors: Arc<Mutex<Vec<PendingVector>>>,
    dropped: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    state_dir: PathBuf,
    pending_path: PathBuf,
    shadow_path: PathBuf,
}

fn worker_loop(rx: Receiver<WorkerMsg>, handles: WorkerHandles) {
    let mut buffer: Vec<TurnEvidence> = Vec::new();
    let mut last_evidence_ms = now_ms();
    let mut candidates = load_candidates(&handles.state_dir);
    let mut turns_seen: u64 = 0;

    loop {
        if handles.shutdown.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(WorkerMsg::Evidence(ev)) => {
                let urgent = looks_like_self_statement(&ev.user_text);
                turns_seen += 1;
                buffer.push(ev);
                last_evidence_ms = now_ms();
                if urgent || buffer.len() >= WINDOW_TURNS {
                    flush_window(
                        &mut buffer,
                        &mut candidates,
                        &handles,
                        turns_seen,
                        urgent,
                        false,
                        &mut last_evidence_ms,
                    );
                }
            }
            Ok(WorkerMsg::Boundary(boundary)) => {
                flush_window(
                    &mut buffer,
                    &mut candidates,
                    &handles,
                    turns_seen,
                    false,
                    boundary == MemorySessionBoundary::Exited,
                    &mut last_evidence_ms,
                );
                last_evidence_ms = now_ms();
            }
            Err(RecvTimeoutError::Timeout) => {
                if !buffer.is_empty() && now_ms() - last_evidence_ms >= WINDOW_IDLE_MS as i64 {
                    flush_window(
                        &mut buffer,
                        &mut candidates,
                        &handles,
                        turns_seen,
                        false,
                        false,
                        &mut last_evidence_ms,
                    );
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_window(
    buffer: &mut Vec<TurnEvidence>,
    candidates: &mut HashMap<String, CandidateEntry>,
    handles: &WorkerHandles,
    _turns_seen: u64,
    urgent: bool,
    clear_pending: bool,
    last_evidence_ms: &mut i64,
) {
    *last_evidence_ms = now_ms();
    if buffer.is_empty() {
        if clear_pending {
            let _ = std::fs::remove_file(&handles.pending_path);
        }
        return;
    }
    let window: Vec<TurnEvidence> = std::mem::take(buffer);
    prune_candidates(candidates);
    save_candidates(&handles.state_dir, candidates);

    let Some(client) = GLOBAL_EXTRACTOR.get().and_then(|slot| slot.as_ref()) else {
        return;
    };
    let prompt = build_extraction_prompt(&window, urgent);
    let Some(ops) = extract_with_retry(client.as_ref(), &prompt) else {
        handles.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let mut accepted: Vec<MemoryOp> = Vec::new();
    for op in ops {
        let Some(op) = op.sanitize() else { continue };
        match gate_op(op, candidates) {
            Gate::Pass(op) => accepted.push(op),
            Gate::Pending => {}
        }
    }
    if accepted.is_empty() {
        return;
    }

    let mode = memory_mode();
    if mode != MemoryMode::Write {
        // log-only / visible：都留一份可审提议，记录本轮窗口覆盖的 turn 范围。
        let (session_id, from_turn, to_turn) = match (window.first(), window.last()) {
            (Some(first), Some(last)) => {
                (last.session_id.clone(), first.turn_index, last.turn_index)
            }
            _ => (String::new(), 0, 0),
        };
        let proposals: Vec<ShadowProposal> = accepted
            .iter()
            .map(|op| ShadowProposal {
                ts_ms: now_ms(),
                session_id: session_id.clone(),
                from_turn,
                to_turn,
                op: op.clone(),
            })
            .collect();
        append_shadow_proposals(&handles.shadow_path, &proposals);
    }

    let provider = crate::build_embedding_provider();
    // log-only 不进队列：只有 visible（标 shadow、进内存投影）与 write 才允许生效。
    if mode != MemoryMode::LogOnly {
        if let Ok(mut queue) = handles.ops.lock() {
            for op in &accepted {
                if op.op == OpKind::Upsert {
                    if let Some(provider) = &provider {
                        if let Ok(vector) = provider.embed(&op.content) {
                            if !vector.is_empty() {
                                if let Ok(mut vectors) = handles.vectors.lock() {
                                    vectors.push((crate::memory::entry_id(&op.content), vector));
                                }
                            }
                        }
                    }
                }
                let mut queued = op.clone();
                if mode == MemoryMode::Visible {
                    queued.shadow = true;
                }
                queue.push(queued);
            }
        }
    }
    if mode == MemoryMode::Write {
        // 追加 op 日志：崩溃后由 load_and_freeze 回放，边界落盘成功后清空。
        let _ = append_ops_log(&handles.pending_path, &accepted);
    }
    save_candidates(&handles.state_dir, candidates);
}

enum Gate {
    Pass(MemoryOp),
    Pending,
}

/// 二现门槛（A2）：`user_stated` / `constraint` / `explicit` 一次即过；
/// `inferred` 需第二次出现。
fn gate_op(op: MemoryOp, candidates: &mut HashMap<String, CandidateEntry>) -> Gate {
    let once_is_enough = op.explicit || matches!(op.basis, Basis::UserStated | Basis::Constraint);
    if once_is_enough {
        return Gate::Pass(op);
    }
    let key = op.dedup_key();
    let now = now_ms();
    match candidates.get_mut(&key) {
        Some(entry) => {
            entry.count = entry.count.saturating_add(1);
            entry.last_seen_ms = now;
            if entry.count >= 2 {
                let mut promoted = op.clone();
                promoted.evidence_count = entry.count;
                candidates.remove(&key);
                Gate::Pass(promoted)
            } else {
                Gate::Pending
            }
        }
        None => {
            if candidates.len() >= CANDIDATE_MAX_ENTRIES {
                return Gate::Pending;
            }
            candidates.insert(
                key,
                CandidateEntry {
                    count: 1,
                    first_seen_ms: now,
                    last_seen_ms: now,
                    kind: op.kind,
                    content: op.content.clone(),
                    confidence: op.confidence,
                    source: op.source.clone(),
                },
            );
            Gate::Pending
        }
    }
}

fn prune_candidates(candidates: &mut HashMap<String, CandidateEntry>) {
    let now = now_ms();
    candidates.retain(|_, entry| now - entry.last_seen_ms <= CANDIDATE_TTL_MS);
    while candidates.len() > CANDIDATE_MAX_ENTRIES {
        let Some(oldest_key) = candidates
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen_ms)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        candidates.remove(&oldest_key);
    }
}

fn load_candidates(state_dir: &Path) -> HashMap<String, CandidateEntry> {
    let path = state_dir.join("memory_candidates.json");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_candidates(state_dir: &Path, candidates: &HashMap<String, CandidateEntry>) {
    let path = state_dir.join("memory_candidates.json");
    if let Ok(raw) = serde_json::to_string(candidates) {
        let _ = std::fs::create_dir_all(state_dir);
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, raw).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn extract_with_retry(client: &dyn MemoryExtractorClient, prompt: &str) -> Option<Vec<MemoryOp>> {
    if let Ok(raw) = client.extract(prompt) {
        if let Some(ops) = parse_ops_response(&raw) {
            return Some(ops);
        }
    }
    let retry_prompt = format!("{prompt}\n\n只输出 JSON，不要任何解释文字。");
    match client.extract(&retry_prompt) {
        Ok(raw) => parse_ops_response(&raw),
        Err(_) => None,
    }
}

fn parse_ops_response(raw: &str) -> Option<Vec<MemoryOp>> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        ops: Vec<MemoryOp>,
    }
    let text = strip_code_fence(raw);
    if text.trim_start().starts_with('[') {
        let start = text.find('[')?;
        let end = text.rfind(']')?;
        if end <= start {
            return None;
        }
        return serde_json::from_str::<Vec<MemoryOp>>(&text[start..=end]).ok();
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let slice = &text[start..=end];
    match serde_json::from_str::<Envelope>(slice) {
        Ok(envelope) => Some(envelope.ops),
        Err(_) => serde_json::from_str::<Vec<MemoryOp>>(slice).ok(),
    }
}

fn strip_code_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let without_open = trimmed
        .trim_start_matches("```json")
        .trim_start_matches("```JSON")
        .trim_start_matches("```");
    without_open
        .rsplit_once("```")
        .map(|(head, _)| head.to_string())
        .unwrap_or_else(|| without_open.to_string())
        .trim()
        .to_string()
}

/// 提取器 prompt（附录 B 的收敛实现）。
fn build_extraction_prompt(window: &[TurnEvidence], urgent: bool) -> String {
    let mut evidence = String::new();
    for ev in window {
        evidence.push_str(&format!(
            "\n[turn {}]\nuser: {}\nassistant: {}\n",
            ev.turn_index, ev.user_text, ev.assistant_text
        ));
        for failure in &ev.tool_failures {
            evidence.push_str(&format!("tool_failure: {failure}\n"));
        }
    }
    format!(
        "你是长期记忆提取器。默认输出 NOOP（{{\"ops\":[]}}）。只有满足下列任一条件才写入：\n\
         1) 稳定偏好：下个月仍成立，且与当前任务无关；\n\
         2) 用户纠正：用户否定先前的说法 / 做法 / 助手行为；\n\
         3) 踩坑根因：失败原因非显然且可复用；\n\
         4) 长期约束：截止、环境、政策、依赖限制。\n\
         \n\
         禁止写入：任务临时状态、一次性指令、可从代码直接读出的事实、密钥/隐私原文。\n\
         每条 op 必须给出 basis：\n\
         - user_stated：用户第一人称自述稳定属性（我喜欢/我讨厌/我通常/我做某类工作）；\n\
         - inferred：从行为或失败模式推测；\n\
         - constraint：不要动某目录 / 必须用某工具 / 环境或政策限制。\n\
         若用户以显式祈使要求记住（记住/以后都/不要再/记错了），explicit=true。\n\
         \n\
         输出严格 JSON：{{\"ops\":[{{\"op\":\"upsert|retract\",\"kind\":\"user_profile|lesson|fact\",\
         \"subject\":\"...\",\"attribute\":\"...\",\"content\":\"<=200字\",\
         \"confidence\":\"high|medium|low\",\"basis\":\"user_stated|inferred|constraint\",\
         \"explicit\":false,\"target\":null}}]}}\n\
         无内容输出 {{\"ops\":[]}}。不要输出解释。\n\
         \n\
         {}\
         本轮用户消息偏短且含第一人称，可能是自述，请优先按 user_stated 判断。\n\
         \n\
         对话证据：{evidence}",
        if urgent {
            "提示：用户可能正在陈述稳定偏好或约束。\n\n"
        } else {
            ""
        }
    )
}

fn append_ops_log(path: &Path, ops: &[MemoryOp]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.len() as u64 > PENDING_LOG_MAX_BYTES {
        let keep_from = existing
            .len()
            .saturating_sub((PENDING_LOG_MAX_BYTES / 2) as usize);
        let boundary = existing
            .char_indices()
            .map(|(idx, _)| idx)
            .find(|idx| *idx >= keep_from)
            .unwrap_or(existing.len());
        existing = existing[boundary..].to_string();
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(existing.as_bytes())?;
    for op in ops {
        if let Ok(line) = serde_json::to_string(op) {
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// 主线程侧的真写入 op 也追加进 journal（崩溃回放用，失败静默）。
pub fn append_journal(memory_path: &Path, ops: &[MemoryOp]) {
    if ops.is_empty() {
        return;
    }
    let _ = append_ops_log(&journal_path_for(memory_path), ops);
}

/// 启动时回放 pending 日志；解析失败的行直接跳过。
#[must_use]
pub fn load_pending_ops(path: &Path) -> Vec<MemoryOp> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<MemoryOp>(line.trim()).ok())
        .filter_map(MemoryOp::sanitize)
        .collect()
}

fn looks_like_secret(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "api_key",
        "apikey",
        "secret_key",
        "password=",
        "passwd=",
        "token=",
        "bearer ",
        "private key",
        "-----begin",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
        || lower.starts_with("sk-")
        || lower.contains(" sk-")
}

/// 结构性“自述”判定：短文本 + 第一人称。不是记忆关键词表，
/// 只用于让窗口提前冲刷（降低成本浪费与记忆延迟）。
fn looks_like_self_statement(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 160 {
        return false;
    }
    trimmed.contains('我')
        || trimmed.contains(" I ")
        || trimmed.starts_with("I ")
        || trimmed.contains(" my ")
        || trimmed.contains("My ")
}

fn normalize_content(content: &str) -> String {
    content
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation())
        .flat_map(char::to_lowercase)
        .collect()
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_envelope_and_array() {
        let raw = r#"{"ops":[{"op":"upsert","kind":"user_profile","content":"用户偏好深色模式","basis":"user_stated"}]}"#;
        let ops = parse_ops_response(raw).expect("envelope should parse");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].basis, Basis::UserStated);

        let raw_array = r#"[{"op":"upsert","content":"x"}]"#;
        let ops = parse_ops_response(raw_array).expect("array should parse");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].basis, Basis::Inferred);
    }

    #[test]
    fn strips_code_fence() {
        let raw = "```json\n{\"ops\":[]}\n```";
        assert!(parse_ops_response(raw).is_some());
    }

    #[test]
    fn sanitize_rejects_secrets_and_truncates() {
        let secret = MemoryOp {
            content: "api_key=sk-123".to_string(),
            ..MemoryOp::default()
        };
        assert!(secret.sanitize().is_none());

        let long = MemoryOp {
            content: "字".repeat(MAX_CONTENT_CHARS + 50),
            ..MemoryOp::default()
        };
        let sanitized = long.sanitize().expect("should sanitize");
        assert_eq!(sanitized.content.chars().count(), MAX_CONTENT_CHARS);
    }

    #[test]
    fn inferred_requires_two_occurrences() {
        let mut candidates = HashMap::new();
        let op = MemoryOp {
            content: "用户似乎偏好 vim".to_string(),
            basis: Basis::Inferred,
            ..MemoryOp::default()
        };
        assert!(matches!(
            gate_op(op.clone(), &mut candidates),
            Gate::Pending
        ));
        assert!(matches!(gate_op(op, &mut candidates), Gate::Pass(_)));
    }

    #[test]
    fn user_stated_passes_once() {
        let mut candidates = HashMap::new();
        let op = MemoryOp {
            content: "用户是后端工程师".to_string(),
            basis: Basis::UserStated,
            ..MemoryOp::default()
        };
        assert!(matches!(gate_op(op, &mut candidates), Gate::Pass(_)));
        assert!(candidates.is_empty());
    }

    #[test]
    fn explicit_overrides_inferred_gate() {
        let mut candidates = HashMap::new();
        let op = MemoryOp {
            content: "用户要求以后都用 tabs".to_string(),
            basis: Basis::Inferred,
            explicit: true,
            ..MemoryOp::default()
        };
        assert!(matches!(gate_op(op, &mut candidates), Gate::Pass(_)));
    }

    #[test]
    fn pending_ops_roundtrip() {
        let dir = std::env::temp_dir().join(format!("claw-mem-log-{}", now_ms()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("memory_pending.jsonl");
        let op = MemoryOp {
            content: "用户偏好 rust".to_string(),
            basis: Basis::UserStated,
            ..MemoryOp::default()
        };
        append_ops_log(&path, std::slice::from_ref(&op)).expect("append");
        let loaded = load_pending_ops(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "用户偏好 rust");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
