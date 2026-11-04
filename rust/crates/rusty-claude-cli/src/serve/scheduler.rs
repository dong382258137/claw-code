//! S2b：Web turn 调度骨架。
//!
//! 把「排队」实体化为三层：
//! - 全局并发门闸（默认 4，环境变量 `CLAW_WEB_MAX_CONCURRENCY` 可覆盖）；
//! - 每工作区门闸（默认 1，`CLAW_WEB_MAX_PER_WORKSPACE` 可覆盖）；
//! - 每会话 prompt 队列（上限 5，超出由调用方返回 429）。
//!
//! 会话状态只有 `idle` / `queued` / `running` 三种；`POST /prompt` 永远
//! 立刻返回 202 + 排队位置，等待许可发生在 worker 线程里，不再把
//! 「等待工作区锁」这类架构错误抛给用户。
//!
//! **槽位所有权归调度器**：一轮开始执行时，全局/工作区许可移交给
//! [`SessionSlot::running`]，不再由 worker 线程持有。这样看门狗超时后可以
//! 直接摘除运行句柄、立即归还槽位，并递增 `generation` 作废僵死 worker
//! 将要发出的迟到事件（worker 侧按 generation 丢弃）。
//!
//! 事件按会话隔离：每个会话一条 `broadcast`，订阅端只拿到本会话数据；
//! 事件带单调 `seq`，缓冲不足时显式发 `reset`，不再静默丢弃 token。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use runtime::HookAbortSignal;

/// 每会话等待队列上限（不含正在执行的那一轮）。
pub const MAX_QUEUE_PER_SESSION: usize = 5;

/// 常驻 worker 上限：超出后按 LRU 回收最久未活动的空闲 worker。
pub const MAX_HOT_WORKERS: usize = 8;

/// worker 空闲多久后退出线程、释放运行态（LiveCli / 拓扑 / MCP）。
pub const WORKER_IDLE_TIMEOUT_MS: u64 = 90_000;

/// worker 空闲轮询间隔。
pub const WORKER_POLL_MS: u64 = 200;

/// 僵死（已被看门狗回收、但线程尚未退出）worker 的上限。
///
/// 回收只归还槽位，线程本身只能靠进程内取消信号自行收尾；超过该数量说明
/// 有线程长期卡在不可中断的工具调用里，继续接收新 prompt 只会堆积资源，
/// 因此直接向调用方返回明确错误。
pub const MAX_ORPHAN_WORKERS: usize = 4;

/// 每会话事件缓冲；超出后订阅端会收到 `reset` 事件。
const EVENT_BUFFER: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Queued,
    Running,
}

impl SessionState {
    pub fn label(self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Queued => "queued",
            SessionState::Running => "running",
        }
    }
}

/// 并发配置。默认全局 4 / 每工作区 1，均可用环境变量临时覆盖。
#[derive(Clone, Copy, Debug)]
pub struct ConcurrencyConfig {
    pub global: usize,
    pub per_workspace: usize,
}

impl ConcurrencyConfig {
    pub fn from_env() -> Self {
        Self {
            global: env_permits("CLAW_WEB_MAX_CONCURRENCY", 4),
            per_workspace: env_permits("CLAW_WEB_MAX_PER_WORKSPACE", 1),
        }
    }
}

fn env_permits(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

/// 一次 Web turn 的输入（全部拥有所有权，便于放进队列）。
pub struct PromptJob {
    pub session_id: String,
    pub session_path: PathBuf,
    pub workspace: PathBuf,
    pub text: String,
    pub permission: runtime::PermissionMode,
    pub model: Option<String>,
}

/// 已接受的排队结果：`position` 从 1 开始（第 1 位即最先执行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Enqueued {
    pub position: usize,
    /// 调用方据此启动该会话的 dispatcher 线程。
    pub start_worker: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueError {
    QueueFull,
    /// 僵死 worker 已达上限，拒绝继续接收新 prompt。
    OrphanLimit {
        active: usize,
        limit: usize,
    },
}

/// 从队列取出的一轮：调用方拿到 job、中止信号和收尾权。
pub struct TakenPrompt {
    pub job: PromptJob,
    pub signal: HookAbortSignal,
    /// 与看门狗共享的「本轮已收尾」标志。
    pub finished: Arc<AtomicBool>,
    /// 流式事件心跳，供会话级看门狗判断是否还有进展。
    pub activity: Arc<AtomicU64>,
    /// 本轮代际；事件发送与收尾都必须带上它，代际不匹配即视为迟到/已回收。
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionStatus {
    pub state: SessionState,
    /// 会话当前队首的序号：有任务时为 1，空闲为 0。
    pub position: usize,
    /// 等待执行的条数（不含正在执行的那一轮）。
    pub queued: usize,
}

#[derive(Default)]
pub struct Interrupted {
    /// 是否有正在执行（或已出队等待许可）的一轮被中止。
    pub aborted: bool,
    /// 被清除的排队条数。
    pub dropped: usize,
}

/// 看门狗扫描到的一轮超时。
pub struct ExpiredTurn {
    pub session: String,
    pub generation: u64,
    pub signal: HookAbortSignal,
    pub finished: Arc<AtomicBool>,
    pub silent_ms: u64,
}

/// 看门狗回收一轮后拿到的信息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReclaimedTurn {
    /// 该会话剩余等待条数（>0 时调用方应补一个 worker 继续消费）。
    pub queued: usize,
}

/// 一轮正常收尾的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// 正常收尾（含被 interrupt 中止的一轮）。
    Done,
    /// 本轮已被看门狗回收，代际已失效；worker 应直接退出线程。
    Orphan,
}

// ---------------- 并发门闸 ---------------------------

struct Gate {
    available: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            available: Mutex::new(permits.max(1)),
            cv: Condvar::new(),
        })
    }

    fn acquire(self: &Arc<Self>) -> GatePermit {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *available == 0 {
            available = self
                .cv
                .wait(available)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *available -= 1;
        GatePermit {
            gate: Arc::clone(self),
        }
    }
}

/// RAII 许可：drop 时归还并唤醒一个等待者。
pub struct GatePermit {
    gate: Arc<Gate>,
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        let mut available = self
            .gate
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available += 1;
        self.gate.cv.notify_one();
    }
}

// ---------------- 调度器 ---------------------------

/// 调度器持有的运行句柄：一轮执行期间的全局 + 工作区许可。
///
/// worker 线程不再持有它，因此看门狗可在超时后直接摘除并释放槽位。
struct RunningHandle {
    #[allow(dead_code)]
    permits: (GatePermit, GatePermit),
}

struct SessionSlot {
    state: SessionState,
    queue: VecDeque<PromptJob>,
    /// 队首已出队、但尚未拿到并发许可。
    pending_head: bool,
    /// 该会话有 worker 线程在跑（可能正阻塞等许可）。
    worker_present: bool,
    /// 当前一轮的中止信号（等待许可阶段也已注册）。
    signal: Option<HookAbortSignal>,
    activity: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
    /// 本轮代际：每次出队与看门狗回收都会递增。
    generation: u64,
    /// 运行句柄（许可归调度器所有，不由 worker 持有）。
    running: Option<RunningHandle>,
    /// 最近一轮是否被看门狗强制标记为失败（诊断用，不改变对外的状态标签）。
    #[allow(dead_code)]
    failed: bool,
}

impl Default for SessionSlot {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
            queue: VecDeque::new(),
            pending_head: false,
            worker_present: false,
            signal: None,
            activity: Arc::new(AtomicU64::new(now_ms())),
            finished: Arc::new(AtomicBool::new(false)),
            generation: 0,
            running: None,
            failed: false,
        }
    }
}

pub struct Scheduler {
    config: ConcurrencyConfig,
    global: Arc<Gate>,
    workspace_gates: Mutex<HashMap<String, Arc<Gate>>>,
    sessions: Mutex<HashMap<String, SessionSlot>>,
    /// 常驻 worker 注册表：最近活动时间 + 回收停止标志。
    workers: Mutex<HashMap<String, WorkerRecord>>,
    /// 已被看门狗回收、但线程尚未退出的 worker 数量。
    orphans: AtomicUsize,
}

#[derive(Clone, Copy)]
struct WorkerRecord {
    last_active_ms: u64,
}

impl Scheduler {
    pub fn new(config: ConcurrencyConfig) -> Self {
        Self {
            config,
            global: Gate::new(config.global),
            workspace_gates: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            workers: Mutex::new(HashMap::new()),
            orphans: AtomicUsize::new(0),
        }
    }

    /// 入队。`start_worker=true` 时调用方需要为该会话启动 dispatcher。
    pub fn enqueue(&self, job: PromptJob) -> Result<Enqueued, EnqueueError> {
        let active = self.orphans.load(Ordering::SeqCst);
        if active >= MAX_ORPHAN_WORKERS {
            return Err(EnqueueError::OrphanLimit {
                active,
                limit: MAX_ORPHAN_WORKERS,
            });
        }
        let session = job.session_id.clone();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = sessions.entry(session).or_default();
        if slot.queue.len() >= MAX_QUEUE_PER_SESSION {
            return Err(EnqueueError::QueueFull);
        }
        // 队首序号 = 已排在他前面的条数 + 1（正在执行的那轮也算一位）。
        let ahead = slot.queue.len()
            + usize::from(slot.state == SessionState::Running || slot.pending_head);
        slot.queue.push_back(job);
        if slot.state == SessionState::Idle {
            slot.state = SessionState::Queued;
        }
        let start_worker = if slot.worker_present {
            false
        } else {
            slot.worker_present = true;
            true
        };
        Ok(Enqueued {
            position: ahead + 1,
            start_worker,
        })
    }

    /// 常驻 worker 从队列取下一条；没有工作时保持 worker 存活（不摘登记）。
    ///
    /// 出队即递增 `generation`，本轮之后的一切事件与收尾都以该代际为准。
    pub fn take_next(&self, session: &str) -> Option<TakenPrompt> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = sessions.get_mut(session)?;
        let Some(job) = slot.queue.pop_front() else {
            slot.state = SessionState::Idle;
            slot.pending_head = false;
            slot.signal = None;
            return None;
        };
        slot.generation += 1;
        let generation = slot.generation;
        let signal = HookAbortSignal::new();
        let finished = Arc::new(AtomicBool::new(false));
        slot.signal = Some(signal.clone());
        slot.finished = Arc::clone(&finished);
        slot.pending_head = true;
        slot.state = SessionState::Queued;
        Some(TakenPrompt {
            job,
            signal,
            finished,
            activity: Arc::clone(&slot.activity),
            generation,
        })
    }

    /// 拿到并发许可并把许可移交给调度器，成功后本轮才真正进入 running。
    ///
    /// 许可在此期间仍由本线程阻塞获取，但一旦拿到就立刻放进 `running`，
    /// 看门狗因此可以在超时后直接摘除它并归还槽位。
    ///
    /// 返回 `false` 表示等待许可期间本轮已被回收（代际失效），调用方应放弃。
    pub fn begin_turn(&self, session: &str, generation: u64, workspace: &Path) -> bool {
        let permits = self.acquire_permits(workspace);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = match sessions.get_mut(session) {
            Some(slot) => {
                if slot.generation != generation {
                    false
                } else {
                    slot.state = SessionState::Running;
                    slot.pending_head = false;
                    slot.failed = false;
                    slot.activity.store(now_ms(), Ordering::SeqCst);
                    slot.running = Some(RunningHandle { permits });
                    true
                }
            }
            None => false,
        };
        if !current {
            drop(sessions);
        }
        current
    }

    /// 本轮代际是否仍然有效。worker 的流式事件与收尾都先过这道门。
    pub fn is_current(&self, session: &str, generation: u64) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .is_some_and(|slot| slot.generation == generation)
    }

    /// worker 正常收尾：归还运行句柄（释放槽位）并清理会话状态。
    ///
    /// 代际不匹配说明本轮已被看门狗回收，此时只递减孤儿计数，绝不触碰
    /// 新会话状态（避免把后来者覆盖成 idle）。
    pub fn complete_turn(&self, session: &str, generation: u64) -> TurnOutcome {
        let released = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(slot) = sessions.get_mut(session) else {
                return TurnOutcome::Orphan;
            };
            if slot.generation != generation {
                drop(sessions);
                let remaining = self.orphans.load(Ordering::SeqCst).saturating_sub(1);
                self.orphans.store(remaining, Ordering::SeqCst);
                return TurnOutcome::Orphan;
            }
            slot.signal = None;
            slot.pending_head = false;
            slot.state = if slot.queue.is_empty() {
                SessionState::Idle
            } else {
                SessionState::Queued
            };
            slot.running.take()
        };
        // 许可在锁外销毁，避免唤醒等待者时仍持有会话表锁。
        drop(released);
        TurnOutcome::Done
    }

    /// 看门狗回收：标记 failed、摘除运行句柄并归还槽位、代际 +1。
    ///
    /// 返回 `None` 表示这一轮已经自行收尾（或状态已不是 running）。
    pub fn reclaim(&self, session: &str, generation: u64) -> Option<ReclaimedTurn> {
        let (released, queued) = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let slot = sessions.get_mut(session)?;
            if slot.generation != generation || slot.state != SessionState::Running {
                return None;
            }
            slot.failed = true;
            slot.generation += 1;
            slot.signal = None;
            slot.pending_head = false;
            slot.worker_present = false;
            let queued = slot.queue.len();
            slot.state = if queued == 0 {
                SessionState::Idle
            } else {
                SessionState::Queued
            };
            (slot.running.take(), queued)
        };
        // 释放槽位在锁外完成：新 prompt 可立刻进入 running。
        drop(released);
        self.orphans.fetch_add(1, Ordering::SeqCst);
        Some(ReclaimedTurn { queued })
    }

    /// 原子认领 worker 启动权。看门狗补 worker 与 `enqueue` 可能同时想启动，
    /// 只有一方能拿到 `true`，保证同一会话永远只有一个 worker 消费队列。
    pub fn claim_worker(&self, session: &str) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = sessions.entry(session.to_string()).or_default();
        if slot.worker_present {
            false
        } else {
            slot.worker_present = true;
            true
        }
    }

    /// 当前僵死 worker 数量（诊断用）。
    pub fn orphan_count(&self) -> usize {
        self.orphans.load(Ordering::SeqCst)
    }

    /// 看门狗强制收尾（仅用于线程启动失败等异常路径）：不递增代际。
    pub fn force_finish(&self, session: &str) {
        let released = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match sessions.get_mut(session) {
                None => None,
                Some(slot) => {
                    slot.signal = None;
                    slot.pending_head = false;
                    slot.state = if slot.queue.is_empty() {
                        SessionState::Idle
                    } else {
                        SessionState::Queued
                    };
                    slot.running.take()
                }
            }
        };
        drop(released);
    }

    /// 登记常驻 worker；超出 [`MAX_HOT_WORKERS`] 时按 LRU 移除最久未活动者
    /// 的登记（对应 worker 会在队列空闲时自行退出并释放运行态）。
    pub fn register_worker(&self, session: &str) {
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        workers.insert(
            session.to_string(),
            WorkerRecord {
                last_active_ms: now_ms(),
            },
        );
        while workers.len() > MAX_HOT_WORKERS {
            let victim = workers
                .iter()
                .filter(|(id, _)| id.as_str() != session)
                .min_by_key(|(_, record)| record.last_active_ms)
                .map(|(id, _)| id.clone());
            let Some(victim) = victim else { break };
            workers.remove(&victim);
        }
    }

    /// 该会话是否仍登记着常驻 worker（测试与诊断用）。
    pub fn has_worker(&self, session: &str) -> bool {
        self.workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(session)
    }

    /// 刷新 worker 最近活动时间（每轮开始与结束时调用），避免空闲计时误杀。
    pub fn touch_worker(&self, session: &str) {
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = workers.get_mut(session) {
            record.last_active_ms = now_ms();
        }
    }

    /// 该 worker 是否应退出：队列必须为空，且登记已被 LRU 移除或空闲超时。
    pub fn worker_should_exit(&self, session: &str, idle_ms: u64) -> bool {
        {
            let sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match sessions.get(session) {
                None => return true,
                Some(slot) if !slot.queue.is_empty() => return false,
                Some(_) => {}
            }
        }
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = workers.get(session) else {
            return true;
        };
        now_ms().saturating_sub(record.last_active_ms) >= idle_ms
    }

    /// 原子退出：队列确实为空时才摘登记并回到 idle。
    ///
    /// 返回 `false` 表示恰好有新任务入队，worker 应继续接单而不是退出。
    pub fn worker_exit(&self, session: &str) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match sessions.get_mut(session) {
            None => {}
            Some(slot) if !slot.queue.is_empty() => return false,
            Some(slot) => {
                slot.worker_present = false;
                slot.state = SessionState::Idle;
            }
        }
        drop(sessions);
        self.workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session);
        true
    }

    /// 线程启动失败等异常场景：强制摘除登记，允许后续入队重新拉起 worker。
    pub fn discard_worker(&self, session: &str) {
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(slot) = sessions.get_mut(session) {
                slot.worker_present = false;
            }
        }
        self.workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session);
    }

    pub fn status(&self, session: &str) -> SessionStatus {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = sessions.get(session) else {
            return SessionStatus {
                state: SessionState::Idle,
                position: 0,
                queued: 0,
            };
        };
        SessionStatus {
            state: slot.state,
            position: if slot.state == SessionState::Idle {
                0
            } else {
                1
            },
            queued: slot.queue.len(),
        }
    }

    pub fn is_active(&self, session: &str) -> bool {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions
            .get(session)
            .is_some_and(|slot| slot.state != SessionState::Idle || !slot.queue.is_empty())
    }

    /// 全服务是否已经安静（无执行中、无待执行）——热升级的前置条件。
    pub fn is_quiet(&self) -> bool {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.values().all(|slot| {
            slot.state == SessionState::Idle && slot.queue.is_empty() && !slot.pending_head
        })
    }

    pub fn interrupt(&self, session: &str) -> Interrupted {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = sessions.get_mut(session) else {
            return Interrupted::default();
        };
        let dropped = slot.queue.len();
        slot.queue.clear();
        let aborted = slot.signal.take().is_some_and(|signal| {
            signal.abort();
            true
        });
        // 只清掉排队项且没有在跑/待许可的一轮时，会话直接回到 idle。
        if !aborted && !slot.pending_head {
            slot.state = SessionState::Idle;
        }
        Interrupted { aborted, dropped }
    }

    /// 会话删除后回收状态与事件通道（删除前已保证不活跃）。
    pub fn forget(&self, session: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.remove(session);
    }

    /// 取工作区 + 全局许可。固定「先工作区后全局」：同一工作区的后来者
    /// 在工作区门闸上等待，不会先占住全局槽位再去等工作区，从而把全局
    /// 名额浪费在排队上。
    pub fn acquire_permits(&self, workspace: &Path) -> (GatePermit, GatePermit) {
        let key = workspace_key(workspace);
        let gate = {
            let mut gates = self
                .workspace_gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(
                gates
                    .entry(key)
                    .or_insert_with(|| Gate::new(self.config.per_workspace)),
            )
        };
        let workspace_permit = gate.acquire();
        let global = self.global.acquire();
        (global, workspace_permit)
    }

    /// 扫描超过阈值没有流式事件的在跑轮次。
    pub fn expired(&self, threshold_ms: u64) -> Vec<ExpiredTurn> {
        let now = now_ms();
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut expired = Vec::new();
        for (session, slot) in sessions.iter() {
            if slot.state != SessionState::Running {
                continue;
            }
            let silent_ms = now.saturating_sub(slot.activity.load(Ordering::SeqCst));
            if silent_ms < threshold_ms {
                continue;
            }
            let Some(signal) = slot.signal.clone() else {
                continue;
            };
            expired.push(ExpiredTurn {
                session: session.clone(),
                generation: slot.generation,
                signal,
                finished: Arc::clone(&slot.finished),
                silent_ms,
            });
        }
        expired
    }

    #[cfg(test)]
    fn queued_len(&self, session: &str) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .map_or(0, |slot| slot.queue.len())
    }
}

fn workspace_key(workspace: &Path) -> String {
    workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .display()
        .to_string()
}

// ---------------- 每会话事件总线 ---------------------------

struct Channel {
    sender: tokio::sync::broadcast::Sender<Value>,
    seq: u64,
}

/// 每个会话一条 broadcast：订阅端不会收到别的会话事件，也不会因为全网
/// 事件过多被挤掉本会话的 token。
#[derive(Default)]
pub struct EventHub {
    channels: Mutex<HashMap<String, Channel>>,
}

impl EventHub {
    pub fn subscribe(&self, session: &str) -> tokio::sync::broadcast::Receiver<Value> {
        let mut channels = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        channels
            .entry(session.to_string())
            .or_insert_with(|| {
                let (sender, _) = tokio::sync::broadcast::channel(EVENT_BUFFER);
                Channel { sender, seq: 0 }
            })
            .sender
            .subscribe()
    }

    /// 发一条本会话事件；自动补齐 `session` 与单调 `seq`。
    pub fn emit(&self, session: &str, mut payload: Value) {
        let mut channels = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let channel = channels.entry(session.to_string()).or_insert_with(|| {
            let (sender, _) = tokio::sync::broadcast::channel(EVENT_BUFFER);
            Channel { sender, seq: 0 }
        });
        channel.seq += 1;
        payload["session"] = Value::String(session.to_string());
        payload["seq"] = json!(channel.seq);
        let _ = channel.sender.send(payload);
    }

    /// 缓冲不足时给这一个订阅端补一条 `reset`；序号继续单调递增。
    pub fn next_reset(&self, session: &str, dropped: usize) -> Value {
        let mut channels = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let channel = channels.entry(session.to_string()).or_insert_with(|| {
            let (sender, _) = tokio::sync::broadcast::channel(EVENT_BUFFER);
            Channel { sender, seq: 0 }
        });
        channel.seq += 1;
        json!({
            "session": session,
            "kind": "reset",
            "dropped": dropped,
            "seq": channel.seq,
        })
    }

    pub fn forget(&self, session: &str) {
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(session: &str, text: &str) -> PromptJob {
        PromptJob {
            session_id: session.to_string(),
            session_path: PathBuf::from("session.jsonl"),
            workspace: PathBuf::from("."),
            text: text.to_string(),
            permission: runtime::PermissionMode::ReadOnly,
            model: None,
        }
    }

    #[test]
    fn enqueue_reports_increasing_positions_per_session() {
        let scheduler = Scheduler::new(ConcurrencyConfig {
            global: 1,
            per_workspace: 1,
        });
        let first = scheduler.enqueue(job("s1", "a")).expect("first accepted");
        assert_eq!(first.position, 1);
        assert!(first.start_worker);
        // 模拟队首已被 worker 取走并开跑。
        let taken = scheduler.take_next("s1").expect("head taken");
        assert!(scheduler.begin_turn("s1", taken.generation, Path::new("w1")));
        let second = scheduler.enqueue(job("s1", "b")).expect("second accepted");
        assert_eq!(second.position, 2);
        assert!(!second.start_worker);
        let third = scheduler.enqueue(job("s1", "c")).expect("third accepted");
        assert_eq!(third.position, 3);
        assert_eq!(scheduler.queued_len("s1"), 2);
        assert_eq!(
            scheduler.complete_turn("s1", taken.generation),
            TurnOutcome::Done
        );
    }

    #[test]
    fn enqueue_rejects_when_session_queue_is_full() {
        let scheduler = Scheduler::new(ConcurrencyConfig {
            global: 1,
            per_workspace: 1,
        });
        assert!(scheduler.enqueue(job("s1", "head")).is_ok());
        scheduler.take_next("s1").expect("head taken");
        for index in 0..MAX_QUEUE_PER_SESSION {
            assert!(
                scheduler.enqueue(job("s1", &format!("q{index}"))).is_ok(),
                "第 {index} 条应在上限内"
            );
        }
        assert_eq!(
            scheduler.enqueue(job("s1", "overflow")),
            Err(EnqueueError::QueueFull)
        );
    }

    #[test]
    fn interrupt_clears_queue_and_aborts_running_turn() {
        let scheduler = Scheduler::new(ConcurrencyConfig {
            global: 1,
            per_workspace: 1,
        });
        scheduler.enqueue(job("s1", "head")).expect("accepted");
        let taken = scheduler.take_next("s1").expect("head taken");
        assert!(scheduler.begin_turn("s1", taken.generation, Path::new("w1")));
        scheduler.enqueue(job("s1", "b")).expect("accepted");
        scheduler.enqueue(job("s1", "c")).expect("accepted");
        let interrupted = scheduler.interrupt("s1");
        assert!(interrupted.aborted);
        assert_eq!(interrupted.dropped, 2);
        assert!(taken.signal.is_aborted());
        assert_eq!(scheduler.status("s1").queued, 0);
        assert_eq!(
            scheduler.complete_turn("s1", taken.generation),
            TurnOutcome::Done
        );
    }

    #[test]
    fn resident_worker_survives_finish_and_exits_only_when_idle() {
        let scheduler = Scheduler::new(ConcurrencyConfig {
            global: 1,
            per_workspace: 1,
        });
        scheduler.register_worker("s1");
        scheduler.enqueue(job("s1", "head")).expect("accepted");
        let first = scheduler.take_next("s1").expect("head taken");
        assert!(scheduler.begin_turn("s1", first.generation, Path::new("w1")));
        assert_eq!(
            scheduler.complete_turn("s1", first.generation),
            TurnOutcome::Done
        );

        // 收尾后 worker 仍在场：下一条入队不应再拉起第二个 worker。
        let enqueued = scheduler.enqueue(job("s1", "next")).expect("accepted");
        assert!(!enqueued.start_worker, "常驻 worker 应继续接单");
        assert!(!scheduler.worker_should_exit("s1", 0), "队列非空时不得退出");

        let second = scheduler.take_next("s1").expect("next taken");
        assert!(scheduler.begin_turn("s1", second.generation, Path::new("w1")));
        assert_eq!(
            scheduler.complete_turn("s1", second.generation),
            TurnOutcome::Done
        );
        assert!(
            !scheduler.worker_should_exit("s1", WORKER_IDLE_TIMEOUT_MS),
            "刚收尾不应立刻退出"
        );
        assert!(
            scheduler.worker_should_exit("s1", 0),
            "模拟空闲超时后应允许退出"
        );
        assert!(scheduler.worker_exit("s1"), "队列已空，退出应成功");
        assert!(
            scheduler
                .enqueue(job("s1", "again"))
                .expect("accepted")
                .start_worker,
            "worker 退出后应由下一次入队重新拉起"
        );
    }

    #[test]
    fn hot_worker_registry_evicts_least_recently_active() {
        let scheduler = Scheduler::new(ConcurrencyConfig {
            global: 1,
            per_workspace: 1,
        });
        for index in 0..MAX_HOT_WORKERS {
            std::thread::sleep(std::time::Duration::from_millis(2));
            scheduler.register_worker(&format!("s{index}"));
        }
        scheduler.touch_worker("s0");
        scheduler.register_worker("s9");
        assert!(scheduler.has_worker("s0"), "刚活动的 worker 不应被回收");
        assert!(
            !scheduler.has_worker("s1"),
            "最久未活动的 worker 应被 LRU 回收"
        );
        assert!(scheduler.has_worker("s9"), "新登记的 worker 必须保留");
    }

    #[test]
    fn is_quiet_only_when_nothing_is_pending() {
        let scheduler = Scheduler::new(ConcurrencyConfig {
            global: 1,
            per_workspace: 1,
        });
        assert!(scheduler.is_quiet());
        scheduler.enqueue(job("s1", "head")).expect("accepted");
        assert!(!scheduler.is_quiet());
        let taken = scheduler.take_next("s1").expect("head taken");
        assert!(scheduler.begin_turn("s1", taken.generation, Path::new("w1")));
        assert!(!scheduler.is_quiet());
        assert_eq!(
            scheduler.complete_turn("s1", taken.generation),
            TurnOutcome::Done
        );
        assert!(scheduler.is_quiet());
    }

    #[test]
    fn reclaim_releases_slot_and_invalidates_generation() {
        let scheduler = Arc::new(Scheduler::new(ConcurrencyConfig {
            global: 1,
            per_workspace: 1,
        }));
        scheduler.enqueue(job("s1", "slow")).expect("accepted");
        let taken = scheduler.take_next("s1").expect("head taken");
        assert!(scheduler.begin_turn("s1", taken.generation, Path::new("w1")));
        // 僵死线程不再持有许可：看门狗摘除句柄后槽位立即可用。
        let reclaimed = scheduler.reclaim("s1", taken.generation).expect("回收成功");
        assert_eq!(reclaimed.queued, 0);
        assert!(!scheduler.is_current("s1", taken.generation));
        assert_eq!(scheduler.orphan_count(), 1);

        let other = Arc::clone(&scheduler);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            other.enqueue(job("s2", "next")).expect("accepted");
            let turn = other.take_next("s2").expect("head taken");
            let ok = other.begin_turn("s2", turn.generation, Path::new("w2"));
            sender.send(ok).expect("send");
            if ok {
                other.complete_turn("s2", turn.generation);
            }
        });
        assert!(receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("回收后 2 秒内应拿到被释放的槽位"));
        // 迟到 worker 收尾按孤儿处理，不得把新状态覆盖。
        assert_eq!(
            scheduler.complete_turn("s1", taken.generation),
            TurnOutcome::Orphan
        );
        assert_eq!(scheduler.orphan_count(), 0);
    }

    #[test]
    fn orphan_limit_rejects_new_prompts() {
        let scheduler = Scheduler::new(ConcurrencyConfig {
            global: MAX_ORPHAN_WORKERS,
            per_workspace: 1,
        });
        for index in 0..MAX_ORPHAN_WORKERS {
            let session = format!("s{index}");
            scheduler.enqueue(job(&session, "slow")).expect("accepted");
            let turn = scheduler.take_next(&session).expect("head taken");
            assert!(scheduler.begin_turn(
                &session,
                turn.generation,
                Path::new(&format!("w{index}"))
            ));
            scheduler.reclaim(&session, turn.generation).expect("回收");
        }
        assert_eq!(scheduler.orphan_count(), MAX_ORPHAN_WORKERS);
        assert_eq!(
            scheduler.enqueue(job("fresh", "hi")),
            Err(EnqueueError::OrphanLimit {
                active: MAX_ORPHAN_WORKERS,
                limit: MAX_ORPHAN_WORKERS,
            })
        );
    }

    #[test]
    fn event_hub_isolates_sessions_and_numbers_events() {
        let hub = EventHub::default();
        let mut first = hub.subscribe("a");
        let mut second = hub.subscribe("b");
        hub.emit("a", json!({ "kind": "delta", "text": "x" }));
        hub.emit("b", json!({ "kind": "delta", "text": "y" }));
        let event = first.try_recv().expect("a receives its own event");
        assert_eq!(event["session"], "a");
        assert_eq!(event["seq"], 1);
        assert!(first.try_recv().is_err(), "a 不应收到 b 的事件");
        let event = second.try_recv().expect("b receives its own event");
        assert_eq!(event["session"], "b");
        assert_eq!(event["seq"], 1);
    }

    #[test]
    fn event_hub_reports_lag_with_reset_and_keeps_seq_monotonic() {
        let hub = EventHub::default();
        let mut receiver = hub.subscribe("a");
        for index in 0..(EVENT_BUFFER + 8) {
            hub.emit("a", json!({ "kind": "delta", "text": index }));
        }
        match receiver.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(dropped)) => {
                assert!(dropped > 0);
                let reset = hub.next_reset("a", dropped as usize);
                assert_eq!(reset["kind"], "reset");
                assert_eq!(reset["seq"], EVENT_BUFFER + 9);
            }
            other => panic!("预期 Lagged，实际 {other:?}"),
        }
    }
}
