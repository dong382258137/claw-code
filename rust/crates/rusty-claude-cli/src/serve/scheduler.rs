//! S2b Phase 1：Web turn 调度骨架。
//!
//! 把「排队」实体化为三层：
//! - 全局并发门闸（默认 1，环境变量 `CLAW_WEB_MAX_CONCURRENCY` 可覆盖）；
//! - 每工作区门闸（默认 1，`CLAW_WEB_MAX_PER_WORKSPACE` 可覆盖）；
//! - 每会话 prompt 队列（上限 5，超出由调用方返回 429）。
//!
//! 会话状态只有 `idle` / `queued` / `running` 三种；`POST /prompt` 永远
//! 立刻返回 202 + 排队位置，等待许可发生在 dispatcher 线程里，不再把
//! 「等待工作区锁」这类架构错误抛给用户。
//!
//! 事件按会话隔离：每个会话一条 `broadcast`，订阅端只拿到本会话数据；
//! 事件带单调 `seq`，缓冲不足时显式发 `reset`，不再静默丢弃 token。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use runtime::HookAbortSignal;

/// 每会话等待队列上限（不含正在执行的那一轮）。
pub const MAX_QUEUE_PER_SESSION: usize = 5;

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

/// 并发配置。Phase 1 默认全 1（行为与改造前一致），Phase 2 再调大。
#[derive(Clone, Copy, Debug)]
pub struct ConcurrencyConfig {
    pub global: usize,
    pub per_workspace: usize,
}

impl ConcurrencyConfig {
    pub fn from_env() -> Self {
        Self {
            global: env_permits("CLAW_WEB_MAX_CONCURRENCY", 1),
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
}

/// 从队列取出的一轮：调用方拿到 job、中止信号和收尾权。
pub struct TakenPrompt {
    pub job: PromptJob,
    pub signal: HookAbortSignal,
    /// 与看门狗共享的「本轮已收尾」标志。
    pub finished: Arc<AtomicBool>,
    /// 流式事件心跳，供会话级看门狗判断是否还有进展。
    pub activity: Arc<AtomicU64>,
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
    pub signal: HookAbortSignal,
    pub finished: Arc<AtomicBool>,
    pub silent_ms: u64,
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

struct SessionSlot {
    state: SessionState,
    queue: VecDeque<PromptJob>,
    /// 队首已出队、但尚未拿到并发许可。
    pending_head: bool,
    /// 该会话有 dispatcher 线程在跑（可能正阻塞等许可）。
    worker_present: bool,
    /// 当前一轮的中止信号（等待许可阶段也已注册）。
    signal: Option<HookAbortSignal>,
    activity: Arc<AtomicU64>,
    finished: Arc<AtomicBool>,
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
        }
    }
}

pub struct Scheduler {
    config: ConcurrencyConfig,
    global: Arc<Gate>,
    workspace_gates: Mutex<HashMap<String, Arc<Gate>>>,
    sessions: Mutex<HashMap<String, SessionSlot>>,
}

impl Scheduler {
    pub fn new(config: ConcurrencyConfig) -> Self {
        Self {
            config,
            global: Gate::new(config.global),
            workspace_gates: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 入队。`start_worker=true` 时调用方需要为该会话启动 dispatcher。
    pub fn enqueue(&self, job: PromptJob) -> Result<Enqueued, EnqueueError> {
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

    /// dispatcher 从队列取下一条；没有工作时顺手把 worker 标记清掉。
    pub fn take_next(&self, session: &str) -> Option<TakenPrompt> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = sessions.get_mut(session)?;
        let Some(job) = slot.queue.pop_front() else {
            slot.state = SessionState::Idle;
            slot.pending_head = false;
            slot.worker_present = false;
            slot.signal = None;
            return None;
        };
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
        })
    }

    /// 拿到并发许可、真正开始执行一轮。
    pub fn mark_running(&self, session: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = sessions.get_mut(session) {
            slot.state = SessionState::Running;
            slot.pending_head = false;
            slot.activity.store(now_ms(), Ordering::SeqCst);
        }
    }

    /// dispatcher 正常收尾：清信号；队列非空则继续 queued，否则回到 idle。
    pub fn finish(&self, session: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = sessions.get_mut(session) {
            slot.signal = None;
            slot.pending_head = false;
            if slot.queue.is_empty() {
                slot.state = SessionState::Idle;
                slot.worker_present = false;
            } else {
                slot.state = SessionState::Queued;
            }
        }
    }

    /// 看门狗强制收尾：只释放会话状态，僵死线程仍持有并发许可。
    pub fn force_finish(&self, session: &str) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = sessions.get_mut(session) {
            slot.signal = None;
            slot.pending_head = false;
            slot.state = if slot.queue.is_empty() {
                SessionState::Idle
            } else {
                SessionState::Queued
            };
        }
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

    /// 取全局 + 工作区许可。固定「先全局后工作区」，避免多工作区死锁。
    pub fn acquire_permits(&self, workspace: &Path) -> (GatePermit, GatePermit) {
        let global = self.global.acquire();
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
        // 模拟队首已被 dispatcher 取走并开跑。
        let taken = scheduler.take_next("s1").expect("head taken");
        scheduler.mark_running("s1");
        let second = scheduler.enqueue(job("s1", "b")).expect("second accepted");
        assert_eq!(second.position, 2);
        assert!(!second.start_worker);
        let third = scheduler.enqueue(job("s1", "c")).expect("third accepted");
        assert_eq!(third.position, 3);
        assert_eq!(scheduler.queued_len("s1"), 2);
        drop(taken);
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
        scheduler.mark_running("s1");
        scheduler.enqueue(job("s1", "b")).expect("accepted");
        scheduler.enqueue(job("s1", "c")).expect("accepted");
        let interrupted = scheduler.interrupt("s1");
        assert!(interrupted.aborted);
        assert_eq!(interrupted.dropped, 2);
        assert!(taken.signal.is_aborted());
        assert_eq!(scheduler.status("s1").queued, 0);
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
        scheduler.take_next("s1").expect("head taken");
        scheduler.mark_running("s1");
        assert!(!scheduler.is_quiet());
        scheduler.finish("s1");
        assert!(scheduler.is_quiet());
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
