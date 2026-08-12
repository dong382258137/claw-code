//! SessionBus — 会话级发布/订阅中枢（设计文档
//! `docs/2026-08-11-session-bus-design.md` §2.1）。
//!
//! 让多个会话窗口（TUI 主会话 / 子代理会话 / IDE 多面板 / IM 频道）之间
//! **主动可见**（注册表 + 状态）并**互通交流**（消息路由 + 未读队列）。
//!
//! # 设计要点
//! - 进程内路由：`peers` 注册表 + 每 peer 未读队列（内存实现，Epic 0 范围）。
//! - 默认权限（`deny by default`）：`Main → *`、`Subagent → Main/Subagent` 放行，
//!   其余拒绝；可通过 [`SessionBus::set_allow`] 覆盖。
//! - 防循环：消息 `hop > BUS_MAX_HOP` 丢弃（扩展 loop_detection 的跨会话抑制）。
//! - 锁安全：所有 Mutex 用 `.unwrap_or_else(|e| e.into_inner())`（project_memory L2 规则）。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 消息最大跳数；超过即丢弃（防广播风暴/循环）。
pub const BUS_MAX_HOP: u8 = 3;

/// 单 peer 未读队列上限（审查补充）：TUI 尚未消费 bus 消息时，防止
/// 长会话下 unread 队列无界增长导致内存泄漏；超限丢弃最旧消息。
pub const MAX_UNREAD_PER_PEER: usize = 100;

/// Done 子代理在 peers 表中的保留上限（审查补充 2026-08-12）。
/// 子代理完成（Handoff）后保留为 `Done` 状态供 `/bus list` 与 AI `bus_list`
/// 查看历史；但长会话派发大量子代理会使 peers 表无界膨胀，且每次 `bus_list`
/// 都列出全部历史 Done 子代理浪费 AI 上下文。超限时按 `last_seen_ms` 淘汰最旧
/// 的 Done 子代理（其完整记录仍在 coordinator 与 `.claw/subagents/` handoff 文件）。
pub const MAX_DONE_SUBAGENTS: usize = 200;

/// 外部入口限流：单 peer 每秒最多发布条数（设计文档 §2.5）。
pub const MAX_PUBLISH_PER_SEC: usize = 20;

/// 会话形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PeerKind {
    /// TUI / REPL 主会话。
    Main,
    /// 派生的子代理会话。
    Subagent,
    /// IDE（VS Code 多面板）。
    Ide,
    /// IM（飞书/企业微信频道）。
    Im,
}

impl PeerKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Subagent => "subagent",
            Self::Ide => "ide",
            Self::Im => "im",
        }
    }
}

/// 会话运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerStatus {
    Idle,
    Streaming,
    Blocked,
    Done,
}

impl PeerStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Streaming => "streaming",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

/// 消息种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BusMessageKind {
    /// 状态变更。
    State,
    /// 普通消息。
    Message,
    /// 子代理交接结果。
    Handoff,
    /// 控制命令。
    Command,
}

impl BusMessageKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Message => "message",
            Self::Handoff => "handoff",
            Self::Command => "command",
        }
    }

    /// 统一解析 kind 字符串（Epic 4 统一路由层）：非法/缺省值回退 `Message`。
    ///
    /// 供外部通道（IDE ACP / IM HTTP）复用，消除各端重复的 match 映射。
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        match s {
            "state" => Self::State,
            "handoff" => Self::Handoff,
            "command" => Self::Command,
            _ => Self::Message,
        }
    }
}

/// 对等会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusPeer {
    pub session_id: String,
    /// 人类可读标签，如 "TUI 主会话" / "subagent:api-worker"。
    pub label: String,
    pub kind: PeerKind,
    pub status: PeerStatus,
    /// 发给本 peer 但未读的消息数。
    pub unread: u32,
    /// 最近活跃时间戳（Unix 毫秒）。
    pub last_seen_ms: u128,
    /// 跨进程 peer 的配置文件/事件队列路径（Epic 2+ 使用）。
    pub config_path: Option<PathBuf>,
}

/// 总线消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    /// 来源 session_id（须已注册）。
    pub from: String,
    /// 目标 session_id 或 "*"（广播）。
    pub to: String,
    pub kind: BusMessageKind,
    pub payload: serde_json::Value,
    /// 防循环跳数。
    pub hop: u8,
    /// Unix 毫秒。
    pub ts_ms: u128,
}

impl BusMessage {
    /// 构造一条文本消息（Epic 4 统一路由层）：payload 固定 `{"text": ...}`，
    /// hop=0、ts=now。供外部通道（IDE ACP / IM HTTP）复用，消除各端重复构造。
    #[must_use]
    pub fn text(
        from: impl Into<String>,
        to: impl Into<String>,
        kind: BusMessageKind,
        text: &str,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            kind,
            payload: serde_json::json!({ "text": text }),
            hop: 0,
            ts_ms: now_ms(),
        }
    }

    /// 提取消息文本（payload.text），非文本消息返回空串。
    #[must_use]
    pub fn text_payload(&self) -> &str {
        self.payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }
}

/// 会话总线。
///
/// 线程安全（所有字段为 `Mutex`），可经 [`global`] 全局共享。
#[derive(Default)]
pub struct SessionBus {
    peers: Mutex<HashMap<String, BusPeer>>,
    unread: Mutex<HashMap<String, Vec<BusMessage>>>,
    allow_rules: Mutex<HashMap<(PeerKind, PeerKind), bool>>,
    /// 订阅表：`watcher_session_id → 被观察的 peer session_id 集合`（Epic 1 `/bus watch`）。
    /// 被观察 peer 收到消息时，镜像一份到 watcher 未读队列（`watch_relay` 标记）。
    watch: Mutex<HashMap<String, HashSet<String>>>,
    /// 跨进程文件事件队列根目录（`.claw/bus/`，Epic 2）。`Some` 时启用文件路由：
    /// 未注册目标 → 写入其邮箱目录；广播 → 写入所有已发现的远端邮箱。
    bus_root: Mutex<Option<PathBuf>>,
    /// 外部入口限流（Epic 4 统一路由层）：`from → 近 1s 内发布时间戳列表`。
    /// 供 `publish_text` 使用，防止外部通道（IDE/IM）消息风暴淹没总线。
    publish_times: Mutex<HashMap<String, Vec<u128>>>,
}

/// 文件事件队列文件名序列号（进程内原子递增，配合 ts+pid 保证唯一）。
static FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 远端会话（邮箱发现的 peer）默认 kind：VS Code 面板等 IDE 形态。
const REMOTE_PEER_KIND: PeerKind = PeerKind::Ide;

impl SessionBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册对等会话。重复注册（同 session_id）更新已有条目。
    pub fn register(&self, peer: BusPeer) -> Result<(), String> {
        if peer.session_id.trim().is_empty() {
            return Err("cannot register peer with empty session_id".to_string());
        }
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        peers.insert(peer.session_id.clone(), peer);
        Ok(())
    }

    /// 注销对等会话（同时清空其未读队列）。
    ///
    /// 审查补充（2026-08-12）：一并清理 watch 订阅残留——注销者作为 watcher 的
    /// 整条订阅、以及其他 watcher 对该 target 的引用（否则 stale 条目导致
    /// `watched_peers` 返回已注销 peer；同名重注册会复活旧订阅）；同时清理
    /// `publish_times` 限流窗口（防止外部动态 session_id 长期运行内存增长）。
    pub fn leave(&self, session_id: &str) {
        self.peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        self.unread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        {
            let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
            watch.remove(session_id);
            let mut emptied: Vec<String> = Vec::new();
            for (watcher, set) in watch.iter_mut() {
                set.remove(session_id);
                if set.is_empty() {
                    emptied.push(watcher.clone());
                }
            }
            for watcher in emptied {
                watch.remove(&watcher);
            }
        }
        self.publish_times
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// 更新对等会话状态与活跃时间。
    pub fn update_status(&self, session_id: &str, status: PeerStatus) {
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(peer) = peers.get_mut(session_id) {
            peer.status = status;
            peer.last_seen_ms = now_ms();
        }
    }

    /// 淘汰最旧的 `Done` 子代理，直到不超过 `max_done`（审查补充 2026-08-12）。
    ///
    /// 子代理完成信息由 coordinator / handoff 文件完整保留，从 peers 表移除
    /// 不丢数据，只控制 `/bus list` / `bus_list` 的可见规模。被淘汰 peer 同时
    /// 清理 unread / watch / 限流窗口（复用 [`Self::leave`]，无锁序问题）。
    pub fn prune_done_peers(&self, max_done: usize) {
        let stale: Vec<String> = {
            let peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
            let mut done: Vec<(&String, &BusPeer)> = peers
                .iter()
                .filter(|(_, p)| p.kind == PeerKind::Subagent && p.status == PeerStatus::Done)
                .collect();
            done.sort_by_key(|(_, p)| p.last_seen_ms);
            let overflow = done.len().saturating_sub(max_done);
            done.into_iter()
                .take(overflow)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in stale {
            self.leave(&id);
        }
    }

    /// 覆盖默认权限规则：`from_kind → to_kind` 是否允许。
    pub fn set_allow(&self, from: PeerKind, to: PeerKind, allowed: bool) {
        self.allow_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((from, to), allowed);
    }

    /// 默认权限：`deny by default`。
    /// - `Main → *`：主会话可广播到任意对等。
    /// - `Subagent → Main / Subagent`：子代理与主会话、同侪子代理互通。
    /// - 其余（含 `Im/Ide → *`）默认拒绝，需显式 `set_allow`。
    fn default_allowed(from: PeerKind, to: PeerKind) -> bool {
        match from {
            PeerKind::Main => true,
            PeerKind::Subagent => matches!(to, PeerKind::Main | PeerKind::Subagent),
            PeerKind::Ide | PeerKind::Im => false,
        }
    }

    fn is_allowed(&self, from: PeerKind, to: PeerKind) -> bool {
        let rules = self.allow_rules.lock().unwrap_or_else(|e| e.into_inner());
        rules
            .get(&(from, to))
            .copied()
            .unwrap_or_else(|| Self::default_allowed(from, to))
    }

    /// 发布消息。
    ///
    /// 校验：来源须已注册；`hop > BUS_MAX_HOP` 拒绝；按 `to`（具体 id 或 `*`）路由，
    /// 且 `from_kind → to_kind` 必须被权限放行。成功投递的消息进入目标未读队列。
    /// 广播 `*` 时排除发送者自身（2026-08-12 审查修正）。
    ///
    /// 此外，被 watcher 订阅的 peer 收到消息时，镜像一份到 watcher 未读队列
    /// （`payload.watch_relay = true`，`payload.original_to` 记录原始目标），
    /// 供 `/bus watch` 进入 OutputView。镜像同样受 `from_kind → watcher_kind` 权限约束。
    ///
    /// Epic 2 跨进程路由：未注册的直接目标（或广播）落到 `bus_root` 下的远端
    /// 邮箱（`publish_to_file`，经 `from_kind → Ide` 权限校验）。
    ///
    /// 返回实际送达的 peer 的 session_id 列表（不含 watch 镜像、不含文件投递）。
    pub fn publish(&self, msg: BusMessage) -> Result<Vec<String>, String> {
        let from_kind = {
            let peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
            peers
                .get(&msg.from)
                .ok_or_else(|| {
                    format!("bus publish rejected: sender '{}' not registered", msg.from)
                })?
                .kind
        };
        self.dispatch(msg, from_kind, true)
    }

    // ---- Epic 4：统一外部入口（供 IDE ACP / IM HTTP 通道复用）----
    //
    // 目标：外部通道只调这两个方法即可完成"注册身份 + 发布文本消息"，
    // 消除各端重复的 peer 注册 / kind 映射 / BusMessage 构造 / 权限校验。

    /// 幂等注册外部进程为 `Ide` peer（IDE 面板 / IM hub 外部调用方）。
    ///
    /// 已注册则静默返回；`session_id` 为空返回 `false`。
    pub fn ensure_external_peer(&self, session_id: &str) -> bool {
        if session_id.trim().is_empty() {
            return false;
        }
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        if peers.contains_key(session_id) {
            return true;
        }
        peers.insert(
            session_id.to_string(),
            BusPeer {
                session_id: session_id.to_string(),
                label: format!("external:{session_id}"),
                kind: PeerKind::Ide,
                status: PeerStatus::Idle,
                unread: 0,
                last_seen_ms: now_ms(),
                config_path: None,
            },
        );
        true
    }

    /// 统一外部入口：构造文本消息 + 发布（含权限校验 + Epic 4 限流）。
    ///
    /// - `kind`：见 [`BusMessageKind::from_str`]（缺省/非法回退 `Message`）。
    /// - 限流：同一 `from` 近 1 秒内超过 [`MAX_PUBLISH_PER_SEC`] 条返回 `Err`。
    /// - 返回实际送达的 peer 列表（同 [`Self::publish`]）。
    pub fn publish_text(
        &self,
        from: &str,
        to: &str,
        kind: BusMessageKind,
        text: &str,
    ) -> Result<Vec<String>, String> {
        // 限流检查（窗口清理 + 计数）
        let now = now_ms();
        {
            let mut times = self.publish_times.lock().unwrap_or_else(|e| e.into_inner());
            let window = times.entry(from.to_string()).or_default();
            window.retain(|&t| now.saturating_sub(t) < 1000);
            if window.len() >= MAX_PUBLISH_PER_SEC {
                return Err(format!(
                    "bus rate limited: '{from}' exceeded {MAX_PUBLISH_PER_SEC} msg/s"
                ));
            }
            window.push(now);
        }
        self.publish(BusMessage::text(from, to, kind, text))
    }

    /// 注入远端消息（跨进程文件事件队列消费端，Epic 2）。
    ///
    /// 与 [`Self::publish`] 同路由（进程内投递 + watch 镜像 + 文件转发），但**跳过
    /// 权限重检**：发送端已在原进程经 `from_kind → Ide` 校验，本端注入视为可信传输
    /// （文件层为对等传输，无需二次 deny-by-default——否则 `Ide → Main` 默认拒绝
    /// 会吞掉所有跨进程消息）。发送者须已注册（poller 会先注册远端 peer）。
    pub fn inject(&self, msg: BusMessage) -> Result<Vec<String>, String> {
        let from_kind = {
            let peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
            peers
                .get(&msg.from)
                .ok_or_else(|| {
                    format!("bus inject rejected: sender '{}' not registered", msg.from)
                })?
                .kind
        };
        self.dispatch(msg, from_kind, false)
    }

    /// 共享分发内核：进程内投递 + watch 镜像 + Epic 2 文件转发。
    /// `enforce_permission = false` 时跳过投递权限校验（供远端注入使用）。
    fn dispatch(
        &self,
        msg: BusMessage,
        from_kind: PeerKind,
        enforce_permission: bool,
    ) -> Result<Vec<String>, String> {
        if msg.hop > BUS_MAX_HOP {
            return Err(format!(
                "bus message dropped: hop {} exceeds max {}",
                msg.hop, BUS_MAX_HOP
            ));
        }

        // 进程内投递 + watch 镜像（作用域内持锁，返回后释放再写文件）。
        let (delivered, file_routes) = {
            let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
            let mut unread = self.unread.lock().unwrap_or_else(|e| e.into_inner());
            let mut delivered = Vec::new();
            // 审查补充(2026-08-12):广播 `*` 排除发送者自身——主会话/IM 频道广播
            // 不应把消息回投给自己(否则 TUI drain 显示"来自自己"、IM 频道重复收到
            // 自己发出的消息)。显式 `to = 自身` 仍正常投递。
            let targets: Vec<String> = if msg.to == "*" {
                peers
                    .keys()
                    .filter(|k| k.as_str() != msg.from)
                    .cloned()
                    .collect()
            } else {
                vec![msg.to.clone()]
            };
            for target in targets {
                let Some(peer) = peers.get_mut(&target) else {
                    continue; // 目标未注册（如已退出），静默跳过
                };
                if enforce_permission && !self.is_allowed(from_kind, peer.kind) {
                    continue; // 权限拒绝
                }
                peer.unread = peer.unread.saturating_add(1);
                peer.last_seen_ms = now_ms();
                let queue = unread.entry(target.clone()).or_default();
                queue.push(msg.clone());
                // 防无界增长：超上限丢弃最旧消息（unread 计数保持精确）。
                if queue.len() > MAX_UNREAD_PER_PEER {
                    queue.remove(0);
                }
                delivered.push(target);
            }

            // Watch 镜像（Epic 1 `/bus watch`）：watcher 主动订阅的 peer 收到消息时，
            // 镜像一份到 watcher 未读队列。锁序 peers → unread → watch 与全库一致。
            // watcher 已直接收到（含广播命中）时跳过，避免重复；发送者自身不跳过——
            // 主会话"发消息给被 watch 的 subagent"正是镜像的核心场景（回显发送内容）。
            if !delivered.is_empty() {
                let watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
                if !watch.is_empty() {
                    for (watcher, watched_set) in watch.iter() {
                        if delivered.contains(watcher) {
                            continue;
                        }
                        let Some(watched) = watched_set.iter().find(|t| delivered.contains(t))
                        else {
                            continue;
                        };
                        let Some(wpeer) = peers.get_mut(watcher) else {
                            continue;
                        };
                        if !self.is_allowed(from_kind, wpeer.kind) {
                            continue; // 镜像同样受权限约束（deny by default）
                        }
                        let mut copy = msg.clone();
                        copy.to = watcher.clone();
                        copy.payload["watch_relay"] = serde_json::json!(true);
                        copy.payload["original_to"] = serde_json::json!(watched);
                        wpeer.unread = wpeer.unread.saturating_add(1);
                        wpeer.last_seen_ms = now_ms();
                        let queue = unread.entry(watcher.clone()).or_default();
                        queue.push(copy);
                        if queue.len() > MAX_UNREAD_PER_PEER {
                            queue.remove(0);
                        }
                    }
                }
            }

            // Epic 2 跨进程路由决策（文件写入延迟到锁释放后）。
            let mut file_routes: Vec<BusMessage> = Vec::new();
            if let Some(bus_root) = self
                .bus_root
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                if msg.to == "*" {
                    // 广播：写入所有已发现的远端邮箱（排除 .tmp/.done 与本进程注册 id）
                    if let Ok(entries) = fs::read_dir(&bus_root) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.starts_with('.') || peers.contains_key(&name) {
                                continue;
                            }
                            if !entry.path().is_dir() {
                                continue;
                            }
                            if enforce_permission && !self.is_allowed(from_kind, REMOTE_PEER_KIND) {
                                continue;
                            }
                            let mut copy = msg.clone();
                            copy.to = name.clone();
                            file_routes.push(copy);
                        }
                    }
                } else if !delivered.contains(&msg.to) {
                    // 直接目标未注册：若其邮箱存在则走文件投递
                    let mailbox = Self::mailbox_dir(&bus_root, &msg.to);
                    if mailbox.is_dir()
                        && (!enforce_permission || self.is_allowed(from_kind, REMOTE_PEER_KIND))
                    {
                        file_routes.push(msg.clone());
                    }
                }
            }
            (delivered, file_routes)
        };

        // 文件写入（无锁）：原子写 + rename，失败静默（下次轮询幂等重放）。
        for route in file_routes {
            let Some(bus_root) = self.bus_root() else {
                break;
            };
            let _ = self.publish_to_file(&bus_root, &route);
        }

        Ok(delivered)
    }

    /// 对等会话快照（按 kind 优先级、再按 label 排序），供 Sidebar / `/bus list` 使用。
    #[must_use]
    pub fn peers_snapshot(&self) -> Vec<BusPeer> {
        let peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<BusPeer> = peers.values().cloned().collect();
        list.sort_by_key(|p| (kind_rank(p.kind), p.label.clone()));
        list
    }

    /// 标记某对等会话的消息已读（清空未读队列）。
    pub fn mark_read(&self, session_id: &str) {
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        let mut unread = self.unread.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(peer) = peers.get_mut(session_id) {
            peer.unread = 0;
        }
        unread.remove(session_id);
    }

    /// 查询某对等会话的未读消息（按时间升序）。
    #[must_use]
    pub fn unread_messages(&self, session_id: &str) -> Vec<BusMessage> {
        let unread = self.unread.lock().unwrap_or_else(|e| e.into_inner());
        unread.get(session_id).cloned().unwrap_or_default()
    }

    /// 消费某对等会话的 `Command` 类消息（steer/kill），保留其余消息（Message 等）。
    ///
    /// 审查补充（2026-08-12）：subagent 每轮只消费 Command（steer/kill），原先用
    /// `unread_messages` + `mark_read` 会在消费命令时把同队列的普通消息一并清空
    /// （例如主会话广播 `*` 落入 subagent 的 Message 与后续 steer 命令混在一起，
    /// Message 被静默丢弃）。本方法只取出 Command 并从队列移除，非 Command 消息
    /// 与未读计数保持不变。锁序 peers → unread 与 `dispatch` 一致，避免死锁。
    #[must_use]
    pub fn consume_commands(&self, session_id: &str) -> Vec<BusMessage> {
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        let mut unread = self.unread.lock().unwrap_or_else(|e| e.into_inner());
        let Some(queue) = unread.get_mut(session_id) else {
            return Vec::new();
        };
        let commands: Vec<BusMessage> = queue
            .iter()
            .filter(|m| m.kind == BusMessageKind::Command)
            .cloned()
            .collect();
        if !commands.is_empty() {
            queue.retain(|m| m.kind != BusMessageKind::Command);
            if let Some(peer) = peers.get_mut(session_id) {
                peer.unread = queue.len() as u32;
            }
        }
        commands
    }

    /// 订阅某 peer 的消息流（Epic 1 `/bus watch`）。
    ///
    /// watcher 与 target 均须已注册；不允许观察自身。
    pub fn watch(&self, watcher: &str, target: &str) -> Result<(), String> {
        if watcher == target {
            return Err("cannot watch own session".to_string());
        }
        let peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        if !peers.contains_key(watcher) {
            return Err(format!("watcher '{watcher}' not registered"));
        }
        if !peers.contains_key(target) {
            return Err(format!("target '{target}' not registered"));
        }
        let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        watch
            .entry(watcher.to_string())
            .or_default()
            .insert(target.to_string());
        Ok(())
    }

    /// 取消订阅。幂等：未订阅时静默。
    pub fn unwatch(&self, watcher: &str, target: &str) {
        let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(set) = watch.get_mut(watcher) {
            set.remove(target);
            if set.is_empty() {
                watch.remove(watcher);
            }
        }
    }

    /// 查询某 watcher 当前订阅的 peer 列表（按 session_id 排序）。
    #[must_use]
    pub fn watched_peers(&self, watcher: &str) -> Vec<String> {
        let watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<String> = watch
            .get(watcher)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        list.sort();
        list
    }

    /// 应用 `session_bus.allow` 配置规则（设计文档 §2.5）。
    ///
    /// 每条规则为 `"<from>:<to>"`，`*` 匹配任意 kind；省略冒号视为 `"<from>:*"`。
    /// 例如 `["im:main", "ide:*"]` → 允许 im→main、ide→任意。
    pub fn apply_allow_rules(&self, rules: &[String]) {
        for rule in rules {
            let Some((from_tok, to_tok)) = rule.split_once(':') else {
                continue;
            };
            let Some(from_kinds) = parse_kind_token(from_tok) else {
                continue;
            };
            let Some(to_kinds) = parse_kind_token(to_tok) else {
                continue;
            };
            for from in &from_kinds {
                for to in &to_kinds {
                    self.set_allow(*from, *to, true);
                }
            }
        }
    }

    // ---- Epic 2：跨进程文件事件队列（设计文档 §2.4）----
    //
    // 目录结构（一次消息 = 一个文件，避免并发追加的文件锁竞争）：
    //   {bus_root}/
    //     {session_id}/                     # 会话邮箱（入站消息）
    //       {ts_ms}-{pid}-{seq}.json
    //     .tmp/                             # 原子写暂存区（poller 不扫描）
    //     .done/{session_id}/               # 已消费归档（幂等重放保护）

    /// 启用跨进程文件事件队列根目录。
    pub fn set_bus_root(&self, bus_root: PathBuf) {
        *self.bus_root.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus_root);
    }

    /// 当前文件事件队列根目录（`None` = 未启用跨进程路由）。
    #[must_use]
    pub fn bus_root(&self) -> Option<PathBuf> {
        self.bus_root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 会话邮箱目录：`{bus_root}/{session_id}`。
    #[must_use]
    pub fn mailbox_dir(bus_root: &Path, session_id: &str) -> PathBuf {
        bus_root.join(session_id)
    }

    /// 已消费归档目录：`{bus_root}/.done/{session_id}`。
    #[must_use]
    pub fn done_dir(bus_root: &Path, session_id: &str) -> PathBuf {
        bus_root.join(".done").join(session_id)
    }

    /// 确保邮箱与归档目录存在（幂等）。
    pub fn ensure_mailbox(bus_root: &Path, session_id: &str) -> Result<(), String> {
        fs::create_dir_all(Self::mailbox_dir(bus_root, session_id))
            .map_err(|e| format!("create mailbox dir failed: {e}"))?;
        fs::create_dir_all(Self::done_dir(bus_root, session_id))
            .map_err(|e| format!("create done dir failed: {e}"))?;
        Ok(())
    }

    /// 原子投递：把消息写入目标会话邮箱（`.tmp/` 暂存 + rename，poller 只见完整文件）。
    ///
    /// 每条消息一个独立文件 `{ts_ms}-{pid}-{seq}.json`，天然规避跨进程并发
    /// 追加同一文件导致的 Windows 文件锁/数据交错（[project_memory L3] 临时文件教训：
    /// 暂存区用项目内 `.tmp/` 而非 `%TEMP%`）。
    pub fn publish_to_file(&self, bus_root: &Path, msg: &BusMessage) -> Result<PathBuf, String> {
        if msg.to.is_empty() || msg.to == "*" {
            return Err("publish_to_file requires a concrete target session_id".to_string());
        }
        Self::ensure_mailbox(bus_root, &msg.to)?;
        let name = format!(
            "{}-{}-{}.json",
            msg.ts_ms,
            std::process::id(),
            FILE_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let tmp_dir = bus_root.join(".tmp");
        fs::create_dir_all(&tmp_dir).map_err(|e| format!("create tmp dir failed: {e}"))?;
        let tmp_path = tmp_dir.join(format!("{name}.tmp"));
        let body = serde_json::to_string(msg).map_err(|e| format!("serialize bus message: {e}"))?;
        fs::write(&tmp_path, body).map_err(|e| format!("write tmp message: {e}"))?;
        let final_path = Self::mailbox_dir(bus_root, &msg.to).join(&name);
        fs::rename(&tmp_path, &final_path)
            .map_err(|e| format!("rename message into mailbox: {e}"))?;
        Ok(final_path)
    }

    /// 消费邮箱：读取 `{bus_root}/{session_id}` 下全部消息，解析后归档到
    /// `.done/{session_id}/`（幂等：归档文件不再返回；rename 失败的文件跳过，
    /// 留待下次轮询——写入方写完后不再碰该文件，无锁竞争）。
    ///
    /// 返回按文件名（时间序）排序的消息列表。
    pub fn consume_mailbox(bus_root: &Path, session_id: &str) -> Vec<BusMessage> {
        let mailbox = Self::mailbox_dir(bus_root, session_id);
        let mut entries: Vec<PathBuf> = fs::read_dir(&mailbox)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        let mut messages = Vec::new();
        for path in entries {
            let Ok(body) = fs::read_to_string(&path) else {
                continue; // 读取中（理论上不会发生，写入是原子的），下次重试
            };
            let Ok(msg) = serde_json::from_str::<BusMessage>(&body) else {
                continue; // 损坏文件：跳过
            };
            // 归档到 .done（rename 原子；失败则下次轮询重试）
            let done_dir = Self::done_dir(bus_root, session_id);
            if fs::create_dir_all(&done_dir).is_ok() {
                let file_name = path.file_name().map(|n| n.to_string_lossy().to_string());
                if let Some(file_name) = file_name {
                    let _ = fs::rename(&path, done_dir.join(&file_name));
                }
            }
            messages.push(msg);
        }
        messages
    }

    /// 发现远端会话：扫描 `{bus_root}` 下非 `.tmp`/`.done` 子目录作为邮箱。
    /// 排除本进程已注册的 session 与 `own_session_id`。kind 默认 [`REMOTE_PEER_KIND`]。
    pub fn remote_peers(&self, bus_root: &Path, own_session_id: &str) -> Vec<BusPeer> {
        let local_ids: HashSet<String> = self
            .peers_snapshot()
            .into_iter()
            .map(|p| p.session_id)
            .collect();
        let mut peers = Vec::new();
        let Ok(entries) = fs::read_dir(bus_root) else {
            return peers;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == own_session_id || local_ids.contains(&name) {
                continue;
            }
            if entry.path().is_dir() {
                peers.push(BusPeer {
                    session_id: name.clone(),
                    label: format!("remote:{name}"),
                    kind: REMOTE_PEER_KIND,
                    status: PeerStatus::Idle,
                    unread: 0,
                    last_seen_ms: now_ms(),
                    config_path: Some(entry.path()),
                });
            }
        }
        peers.sort_by_key(|p| p.label.clone());
        peers
    }

    /// 启动邮箱轮询线程（后台，随进程退出终止）。
    ///
    /// 周期性消费 `own_session_id` 的邮箱：来自本进程已注册 peer 的消息（已在
    /// 进程内投递）跳过；来自未知 peer（其他进程）的消息注册为远端 peer 后注入
    /// 本地总线。注入走 [`Self::inject`]（跳过接收端权限重检：发送端已校验，
    /// 否则 `Ide → Main` 默认拒绝会吞掉所有跨进程消息）。
    pub fn start_mailbox_poller(bus_root: PathBuf, own_session_id: String) {
        std::thread::spawn(move || loop {
            let messages = Self::consume_mailbox(&bus_root, &own_session_id);
            if !messages.is_empty() {
                let bus = global();
                for msg in messages {
                    let is_local = bus
                        .peers_snapshot()
                        .iter()
                        .any(|p| p.session_id == msg.from);
                    if is_local {
                        continue; // 本进程消息，已在进程内投递
                    }
                    let _ = bus.register(BusPeer {
                        session_id: msg.from.clone(),
                        label: format!("remote:{}", msg.from),
                        kind: REMOTE_PEER_KIND,
                        status: PeerStatus::Idle,
                        unread: 0,
                        last_seen_ms: now_ms(),
                        config_path: Some(Self::mailbox_dir(&bus_root, &msg.from)),
                    });
                    // 注入（跳过接收端权限重检：发送端已校验；否则 Ide→Main 默认拒绝
                    // 会吞掉所有跨进程消息）。
                    let _ = bus.inject(msg);
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        });
    }
}

/// 对等会话展示排序：Main < Subagent < Ide < Im。
fn kind_rank(kind: PeerKind) -> u8 {
    match kind {
        PeerKind::Main => 0,
        PeerKind::Subagent => 1,
        PeerKind::Ide => 2,
        PeerKind::Im => 3,
    }
}

/// 解析 `session_bus.allow` 规则的 kind token。`*` 返回全部 kind。
/// 非法 token（如 "foo"）返回 `None`。
fn parse_kind_token(token: &str) -> Option<Vec<PeerKind>> {
    match token.trim() {
        "*" => Some(vec![
            PeerKind::Main,
            PeerKind::Subagent,
            PeerKind::Ide,
            PeerKind::Im,
        ]),
        "main" => Some(vec![PeerKind::Main]),
        "subagent" => Some(vec![PeerKind::Subagent]),
        "ide" => Some(vec![PeerKind::Ide]),
        "im" => Some(vec![PeerKind::Im]),
        _ => None,
    }
}

/// 当前 Unix 毫秒。
#[must_use]
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

/// 进程内全局总线实例。
#[must_use]
pub fn global() -> &'static SessionBus {
    static BUS: OnceLock<SessionBus> = OnceLock::new();
    BUS.get_or_init(SessionBus::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, kind: PeerKind) -> BusPeer {
        BusPeer {
            session_id: id.to_string(),
            label: format!("{id} ({})", kind.as_str()),
            kind,
            status: PeerStatus::Idle,
            unread: 0,
            last_seen_ms: now_ms(),
            config_path: None,
        }
    }

    fn msg(from: &str, to: &str, kind: BusMessageKind) -> BusMessage {
        BusMessage {
            from: from.to_string(),
            to: to.to_string(),
            kind,
            payload: serde_json::json!({"hello": "world"}),
            hop: 0,
            ts_ms: now_ms(),
        }
    }

    #[test]
    fn register_leave_and_snapshot() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        let snapshot = bus.peers_snapshot();
        assert_eq!(snapshot.len(), 2);
        // Main 排在 Subagent 前
        assert_eq!(snapshot[0].session_id, "main-1");
        assert_eq!(snapshot[1].session_id, "sub-1");
        bus.leave("sub-1");
        assert_eq!(bus.peers_snapshot().len(), 1);
    }

    #[test]
    fn publish_to_star_delivers_all_allowed() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        // Main 广播 → 送达全部可达 peer,但排除发送者自身(审查修正 2026-08-12)
        let delivered = bus
            .publish(msg("main-1", "*", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(delivered, vec!["sub-1".to_string()]);
        assert!(
            !delivered.contains(&"main-1".to_string()),
            "广播不得回投给自己"
        );
        // 未读递增
        let sub = bus
            .peers_snapshot()
            .into_iter()
            .find(|p| p.session_id == "sub-1")
            .unwrap();
        assert_eq!(sub.unread, 1);
        assert_eq!(bus.unread_messages("sub-1").len(), 1);
        // 主会话自身未收到自己的广播
        assert!(bus.unread_messages("main-1").is_empty());
    }

    #[test]
    fn publish_to_specific_target_only() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        bus.register(peer("sub-2", PeerKind::Subagent)).unwrap();
        let delivered = bus
            .publish(msg("main-1", "sub-2", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(delivered, vec!["sub-2".to_string()]);
    }

    #[test]
    fn unregistered_sender_rejected() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        let err = bus
            .publish(msg("ghost", "main-1", BusMessageKind::Message))
            .unwrap_err();
        assert!(err.contains("not registered"));
    }

    #[test]
    fn default_policy_denies_im_and_ide() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("im-1", PeerKind::Im)).unwrap();
        bus.register(peer("ide-1", PeerKind::Ide)).unwrap();
        // Im → Main 默认拒绝
        let delivered = bus
            .publish(msg("im-1", "main-1", BusMessageKind::Message))
            .expect("publish should not error");
        assert!(delivered.is_empty());
        // Ide → Main 默认拒绝
        let delivered = bus
            .publish(msg("ide-1", "main-1", BusMessageKind::Message))
            .expect("publish should not error");
        assert!(delivered.is_empty());
        // Main → Im 默认放行
        let delivered = bus
            .publish(msg("main-1", "im-1", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(delivered, vec!["im-1".to_string()]);
    }

    #[test]
    fn subagent_to_main_allowed_by_default() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        let delivered = bus
            .publish(msg("sub-1", "main-1", BusMessageKind::Handoff))
            .expect("publish");
        assert_eq!(delivered, vec!["main-1".to_string()]);
    }

    #[test]
    fn explicit_allow_overrides_default_policy() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("im-1", PeerKind::Im)).unwrap();
        bus.set_allow(PeerKind::Im, PeerKind::Main, true);
        let delivered = bus
            .publish(msg("im-1", "main-1", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(delivered, vec!["main-1".to_string()]);
    }

    #[test]
    fn hop_limit_drops_message() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        let mut m = msg("main-1", "sub-1", BusMessageKind::Message);
        m.hop = BUS_MAX_HOP + 1;
        let err = bus.publish(m).unwrap_err();
        assert!(err.contains("hop"));
    }

    #[test]
    fn mark_read_clears_unread() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        bus.publish(msg("main-1", "sub-1", BusMessageKind::Message))
            .unwrap();
        assert_eq!(bus.unread_messages("sub-1").len(), 1);
        bus.mark_read("sub-1");
        assert!(bus.unread_messages("sub-1").is_empty());
        let sub = bus
            .peers_snapshot()
            .into_iter()
            .find(|p| p.session_id == "sub-1")
            .unwrap();
        assert_eq!(sub.unread, 0);
    }

    // 审查补充(内存泄漏防护):unread 队列必须有界,超限丢弃最旧消息。
    #[test]
    fn unread_queue_is_bounded() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        let total = MAX_UNREAD_PER_PEER + 50;
        for _ in 0..total {
            bus.publish(msg("main-1", "sub-1", BusMessageKind::Message))
                .expect("publish");
        }
        assert!(
            bus.unread_messages("sub-1").len() <= MAX_UNREAD_PER_PEER,
            "unread queue must be bounded"
        );
        // unread 计数保持精确(不受丢弃影响)
        let sub = bus
            .peers_snapshot()
            .into_iter()
            .find(|p| p.session_id == "sub-1")
            .unwrap();
        assert_eq!(sub.unread, total as u32);
    }

    #[test]
    fn update_status_changes_peer() {
        let bus = SessionBus::new();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        bus.update_status("sub-1", PeerStatus::Streaming);
        let sub = bus
            .peers_snapshot()
            .into_iter()
            .find(|p| p.session_id == "sub-1")
            .unwrap();
        assert_eq!(sub.status, PeerStatus::Streaming);
    }

    // ---- Epic 1: /bus watch ----

    #[test]
    fn watch_mirrors_message_to_watcher() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        bus.watch("main-1", "sub-1").expect("watch");
        let delivered = bus
            .publish(msg("main-1", "sub-1", BusMessageKind::Command))
            .expect("publish");
        assert_eq!(delivered, vec!["sub-1".to_string()]);
        // watcher 的未读队列收到镜像(带 watch_relay 标记)
        let mirror = bus.unread_messages("main-1");
        assert_eq!(mirror.len(), 1);
        assert_eq!(
            mirror[0]
                .payload
                .get("watch_relay")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            mirror[0]
                .payload
                .get("original_to")
                .and_then(|v| v.as_str()),
            Some("sub-1")
        );
        // 原始目标未读不受镜像影响
        assert_eq!(bus.unread_messages("sub-1").len(), 1);
    }

    #[test]
    fn watch_does_not_mirror_broadcast_already_received() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        bus.watch("main-1", "sub-1").expect("watch");
        // 广播:main-1 已直接收到,不产生重复镜像
        bus.publish(msg("sub-1", "*", BusMessageKind::Handoff))
            .expect("publish");
        assert_eq!(bus.unread_messages("main-1").len(), 1);
    }

    #[test]
    fn watch_rejects_self_unregistered_and_unwatch() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        // 观察自身被拒
        assert!(bus.watch("main-1", "main-1").is_err());
        // 未注册 watcher 被拒
        assert!(bus.watch("ghost", "sub-1").is_err());
        // 未注册 target 被拒
        assert!(bus.watch("main-1", "ghost").is_err());
        // unwatch 幂等
        bus.unwatch("main-1", "sub-1");
        assert!(bus.watched_peers("main-1").is_empty());
        // watch 后 unwatch 生效
        bus.watch("main-1", "sub-1").expect("watch");
        bus.unwatch("main-1", "sub-1");
        assert!(bus.watched_peers("main-1").is_empty());
    }

    #[test]
    fn watched_peers_lists_sorted_subscriptions() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-2", PeerKind::Subagent)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        bus.watch("main-1", "sub-2").expect("watch");
        bus.watch("main-1", "sub-1").expect("watch");
        assert_eq!(bus.watched_peers("main-1"), vec!["sub-1", "sub-2"]);
    }

    // 审查补充(2026-08-12):leave 必须清理 watch 表残留(watcher 条目 + 被观察
    // target 引用),否则 watched_peers 返回已注销 peer、同名重注册复活旧订阅。
    #[test]
    fn leave_cleans_watch_subscriptions() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        bus.register(peer("sub-2", PeerKind::Subagent)).unwrap();
        bus.watch("main-1", "sub-1").expect("watch");
        bus.watch("sub-2", "main-1").expect("watch");
        // 注销 target(sub-1):main-1 的订阅中被移除,集合空 → 条目删除
        bus.leave("sub-1");
        assert!(bus.watched_peers("main-1").is_empty());
        // sub-2 仍在注册中,其订阅(main-1)保留
        assert_eq!(bus.watched_peers("sub-2"), vec!["main-1".to_string()]);
        // 注销 watcher(sub-2):其整条订阅删除
        bus.leave("sub-2");
        assert!(bus.watched_peers("sub-2").is_empty());
        // 注销后同名重注册不再复活旧订阅
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        assert!(bus.watched_peers("main-1").is_empty());
    }

    // 审查补充(2026-08-12):consume_commands 只消费 Command,保留同队列 Message
    // 与未读计数(此前 mark_read 全清会静默丢弃混在一起的普通消息)。
    #[test]
    fn consume_commands_preserves_regular_messages() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        // 先广播 Message(落入 sub-1 队列),再注入 Command(steer)
        bus.publish(msg("main-1", "*", BusMessageKind::Message))
            .expect("broadcast");
        let mut cmd = msg("main-1", "sub-1", BusMessageKind::Command);
        cmd.payload = serde_json::json!({"action": "steer", "message": "go"});
        bus.publish(cmd).expect("publish command");
        // consume_commands:只取出 Command,Message 保留
        let cmds = bus.consume_commands("sub-1");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0].payload.get("action").and_then(|v| v.as_str()),
            Some("steer")
        );
        let remaining = bus.unread_messages("sub-1");
        assert_eq!(remaining.len(), 1, "Message 不得被消费丢弃");
        assert_eq!(remaining[0].kind, BusMessageKind::Message);
        // 未读计数与剩余队列一致
        let sub = bus
            .peers_snapshot()
            .into_iter()
            .find(|p| p.session_id == "sub-1")
            .unwrap();
        assert_eq!(sub.unread, 1);
        // 再次消费:无 Command 返回空,Message 仍保留
        assert!(bus.consume_commands("sub-1").is_empty());
        assert_eq!(bus.unread_messages("sub-1").len(), 1);
    }

    // 审查补充(2026-08-12):Done 子代理达上限后按 last_seen_ms 淘汰最旧,
    // 保留最新的 Done 与仍在运行(Streaming)的子代理。
    #[test]
    fn prune_done_peers_removes_oldest_done_only() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        // 3 个 Done 子代理 + 1 个 Streaming 子代理
        for i in 0..3 {
            let mut p = peer(&format!("sub-done-{i}"), PeerKind::Subagent);
            p.status = PeerStatus::Done;
            p.last_seen_ms = 1000 + i as u128; // done-0 最旧, done-2 最新
            bus.register(p).unwrap();
        }
        let mut running = peer("sub-run", PeerKind::Subagent);
        running.status = PeerStatus::Streaming;
        running.last_seen_ms = 9999;
        bus.register(running).unwrap();
        // 上限 2:淘汰最旧的 done-0
        bus.prune_done_peers(2);
        let ids: Vec<String> = bus
            .peers_snapshot()
            .into_iter()
            .map(|p| p.session_id)
            .collect();
        assert!(
            !ids.contains(&"sub-done-0".to_string()),
            "oldest Done pruned"
        );
        assert!(ids.contains(&"sub-done-1".to_string()));
        assert!(ids.contains(&"sub-done-2".to_string()));
        assert!(
            ids.contains(&"sub-run".to_string()),
            "running peer untouched"
        );
        assert!(ids.contains(&"main-1".to_string()), "main peer untouched");
        // 上限足够大时不淘汰
        bus.register(peer("sub-done-3", PeerKind::Subagent))
            .unwrap();
        bus.prune_done_peers(10);
        assert!(bus
            .peers_snapshot()
            .iter()
            .any(|p| p.session_id == "sub-done-3"));
    }

    #[test]
    fn watch_mirror_respects_permission_policy() {
        let bus = SessionBus::new();
        bus.register(peer("sub-2", PeerKind::Subagent)).unwrap();
        bus.register(peer("im-1", PeerKind::Im)).unwrap();
        bus.register(peer("sub-1", PeerKind::Subagent)).unwrap();
        // im-1 观察 sub-1;sub-2 发消息给 sub-1 时,镜像到 im-1 受默认策略拒绝(Subagent→Im 默认 deny)
        bus.watch("im-1", "sub-1").expect("watch");
        let delivered = bus
            .publish(msg("sub-2", "sub-1", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(delivered, vec!["sub-1".to_string()]);
        // 默认拒绝 Subagent→Im 镜像
        assert!(bus.unread_messages("im-1").is_empty());
        // 显式放行后镜像可达
        bus.set_allow(PeerKind::Subagent, PeerKind::Im, true);
        bus.publish(msg("sub-2", "sub-1", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(bus.unread_messages("im-1").len(), 1);
    }

    // ---- Epic 1: session_bus.allow 配置 ----

    #[test]
    fn apply_allow_rules_grants_configured_pairs() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("im-1", PeerKind::Im)).unwrap();
        bus.apply_allow_rules(&["im:main".to_string()]);
        let delivered = bus
            .publish(msg("im-1", "main-1", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(delivered, vec!["main-1".to_string()]);
    }

    #[test]
    fn apply_allow_rules_star_expands_to_all_kinds() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.register(peer("im-1", PeerKind::Im)).unwrap();
        // 省略冒号视为 from:*;ide → * 放行
        bus.register(peer("ide-1", PeerKind::Ide)).unwrap();
        bus.apply_allow_rules(&["ide:*".to_string()]);
        let delivered = bus
            .publish(msg("ide-1", "main-1", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(delivered, vec!["main-1".to_string()]);
        // 非法规则被忽略
        bus.apply_allow_rules(&["foo:bar".to_string(), "im".to_string()]);
        let delivered = bus
            .publish(msg("im-1", "main-1", BusMessageKind::Message))
            .expect("publish");
        assert!(delivered.is_empty(), "illegal rules must be ignored");
    }

    // ---- Epic 2: 跨进程文件事件队列 ----

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bus-test-{}-{nanos}-{seq}", std::process::id()))
    }

    #[test]
    fn publish_to_file_roundtrips_through_consume_mailbox() {
        let bus = SessionBus::new();
        let root = temp_dir();
        let m = msg("main-1", "ide-1", BusMessageKind::Message);
        let path = bus.publish_to_file(&root, &m).expect("publish_to_file");
        assert!(path.exists(), "message file must exist in mailbox");
        let consumed = SessionBus::consume_mailbox(&root, "ide-1");
        assert_eq!(consumed.len(), 1);
        assert_eq!(consumed[0].from, "main-1");
        assert_eq!(consumed[0].to, "ide-1");
        // 幂等：已归档不再返回
        assert!(SessionBus::consume_mailbox(&root, "ide-1").is_empty());
        // 归档到 .done
        let done = SessionBus::done_dir(&root, "ide-1");
        let archived = fs::read_dir(&done)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        assert!(archived, "consumed message must be archived to .done");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn publish_to_file_rejects_star_and_empty_target() {
        let bus = SessionBus::new();
        let root = temp_dir();
        let mut m = msg("main-1", "*", BusMessageKind::Message);
        assert!(bus.publish_to_file(&root, &m).is_err(), "* target rejected");
        m.to = String::new();
        assert!(
            bus.publish_to_file(&root, &m).is_err(),
            "empty target rejected"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn publish_routes_unregistered_target_to_mailbox() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        let root = temp_dir();
        SessionBus::ensure_mailbox(&root, "ide-1").expect("ensure mailbox");
        bus.set_bus_root(root.clone());
        let delivered = bus
            .publish(msg("main-1", "ide-1", BusMessageKind::Message))
            .expect("publish");
        assert!(
            delivered.is_empty(),
            "unregistered target not in-process delivered"
        );
        let consumed = SessionBus::consume_mailbox(&root, "ide-1");
        assert_eq!(consumed.len(), 1, "message routed to mailbox");
        assert_eq!(consumed[0].to, "ide-1");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn broadcast_writes_to_remote_mailboxes() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        let root = temp_dir();
        SessionBus::ensure_mailbox(&root, "ide-1").expect("ensure mailbox");
        SessionBus::ensure_mailbox(&root, "ide-2").expect("ensure mailbox");
        bus.set_bus_root(root.clone());
        bus.publish(msg("main-1", "*", BusMessageKind::Message))
            .expect("publish");
        assert_eq!(SessionBus::consume_mailbox(&root, "ide-1").len(), 1);
        assert_eq!(SessionBus::consume_mailbox(&root, "ide-2").len(), 1);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn remote_peers_discovers_mailboxes() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        let root = temp_dir();
        SessionBus::ensure_mailbox(&root, "ide-1").expect("ensure mailbox");
        SessionBus::ensure_mailbox(&root, "main-1").expect("ensure mailbox");
        let peers = bus.remote_peers(&root, "main-1");
        assert_eq!(peers.len(), 1, "own session excluded from discovery");
        assert_eq!(peers[0].session_id, "ide-1");
        assert_eq!(peers[0].kind, PeerKind::Ide);
        fs::remove_dir_all(&root).ok();
    }

    /// 端到端跨进程流程(双实例模拟)：进程 A 的 Main 发消息给未注册的 ide-b
    /// → 落 ide-b 邮箱 → 进程 B 消费邮箱并注入本地总线 → B 的 Main peer 收到。
    #[test]
    fn cross_process_flow_a_sends_b_consumes_and_injects() {
        let root = temp_dir();
        // 进程 B:自身会话 id = ide-b(注册为 Main),共享 bus_root
        let bus_b = SessionBus::new();
        bus_b.register(peer("ide-b", PeerKind::Main)).unwrap();
        SessionBus::ensure_mailbox(&root, "ide-b").expect("ensure mailbox");
        bus_b.set_bus_root(root.clone());

        // 进程 A:注册 main-a,向未注册的 ide-b 发布(经文件路由落邮箱)
        let bus_a = SessionBus::new();
        bus_a.register(peer("main-a", PeerKind::Main)).unwrap();
        SessionBus::ensure_mailbox(&root, "main-a").expect("ensure mailbox");
        bus_a.set_bus_root(root.clone());
        let delivered = bus_a
            .publish(msg("main-a", "ide-b", BusMessageKind::Message))
            .expect("publish");
        assert!(delivered.is_empty(), "ide-b 未注册 → 不进程内送达");
        // 文件已落 ide-b 邮箱(进程 A 不消费,只确认投递成功)
        let mailbox = SessionBus::mailbox_dir(&root, "ide-b");
        let files: Vec<_> = fs::read_dir(&mailbox)
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        assert_eq!(files.len(), 1, "消息已落 ide-b 邮箱");

        // 进程 B:消费邮箱 → 注册远端发送方 → 注入本地总线 → 自身 Main(ide-b)收到
        let consumed = SessionBus::consume_mailbox(&root, "ide-b");
        assert_eq!(consumed.len(), 1);
        let injected = &consumed[0];
        let _ = bus_b.register(peer(&injected.from, PeerKind::Ide));
        let delivered_b = bus_b.inject(injected.clone()).expect("inject");
        assert_eq!(delivered_b, vec!["ide-b".to_string()], "注入后 ide-b 收到");
        let unread = bus_b.unread_messages("ide-b");
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].from, "main-a");
        assert_eq!(unread[0].to, "ide-b");
        fs::remove_dir_all(&root).ok();
    }

    // ---- Epic 4:统一外部入口 API ----

    #[test]
    fn ensure_external_peer_is_idempotent() {
        let bus = SessionBus::new();
        // 首次注册
        assert!(bus.ensure_external_peer("ide-panel-1"));
        // 重复注册幂等
        assert!(bus.ensure_external_peer("ide-panel-1"));
        // 空 id 拒绝
        assert!(!bus.ensure_external_peer(""));
        assert!(!bus.ensure_external_peer("   "));
        // 已注册为 Ide kind
        let snapshot = bus.peers_snapshot();
        let p = snapshot
            .iter()
            .find(|p| p.session_id == "ide-panel-1")
            .expect("peer registered");
        assert_eq!(p.kind, PeerKind::Ide);
        assert_eq!(p.label, "external:ide-panel-1");
    }

    #[test]
    fn kind_from_str_maps_all_variants() {
        assert_eq!(BusMessageKind::from_str("state"), BusMessageKind::State);
        assert_eq!(BusMessageKind::from_str("handoff"), BusMessageKind::Handoff);
        assert_eq!(BusMessageKind::from_str("command"), BusMessageKind::Command);
        assert_eq!(BusMessageKind::from_str("message"), BusMessageKind::Message);
        // 非法/缺省回退 Message
        assert_eq!(BusMessageKind::from_str("bogus"), BusMessageKind::Message);
        assert_eq!(BusMessageKind::from_str(""), BusMessageKind::Message);
    }

    #[test]
    fn bus_message_text_constructor_and_payload() {
        let m = BusMessage::text("ide-1", "*", BusMessageKind::Handoff, "hello");
        assert_eq!(m.from, "ide-1");
        assert_eq!(m.to, "*");
        assert_eq!(m.kind, BusMessageKind::Handoff);
        assert_eq!(m.hop, 0);
        assert_eq!(m.text_payload(), "hello");
        // 非文本 payload → 空串
        let m2 = BusMessage {
            from: "a".into(),
            to: "b".into(),
            kind: BusMessageKind::State,
            payload: serde_json::json!({"status": "ok"}),
            hop: 0,
            ts_ms: now_ms(),
        };
        assert_eq!(m2.text_payload(), "");
    }

    #[test]
    fn publish_text_routes_with_permission_and_rate_limit() {
        let bus = SessionBus::new();
        bus.register(peer("main-1", PeerKind::Main)).unwrap();
        bus.ensure_external_peer("ide-1");
        // Ide → Main 默认拒绝(deny by default)
        let delivered = bus
            .publish_text("ide-1", "main-1", BusMessageKind::Message, "hi")
            .expect("publish_text should not error");
        assert!(delivered.is_empty(), "Ide→Main default denied");
        // 放行后送达
        bus.set_allow(PeerKind::Ide, PeerKind::Main, true);
        let delivered = bus
            .publish_text("ide-1", "main-1", BusMessageKind::Message, "hi")
            .expect("publish_text");
        assert_eq!(delivered, vec!["main-1".to_string()]);

        // 限流:同一 from 1s 内超过 MAX 条被拒
        let mut rejected = 0;
        for _ in 0..(MAX_PUBLISH_PER_SEC + 5) {
            if bus
                .publish_text("ide-1", "main-1", BusMessageKind::Message, "spam")
                .is_err()
            {
                rejected += 1;
            }
        }
        assert!(rejected > 0, "rate limit must trigger");
    }

    #[test]
    fn publish_text_rate_limit_is_per_sender() {
        let bus = SessionBus::new();
        // 两个不同 from 互不影响
        bus.ensure_external_peer("ide-a");
        bus.ensure_external_peer("ide-b");
        // 灌满 ide-a 的窗口
        for _ in 0..MAX_PUBLISH_PER_SEC {
            bus.publish_text("ide-a", "*", BusMessageKind::Message, "x")
                .ok();
        }
        // ide-a 超限
        assert!(bus
            .publish_text("ide-a", "*", BusMessageKind::Message, "y")
            .is_err());
        // ide-b 不受影响
        assert!(bus
            .publish_text("ide-b", "*", BusMessageKind::Message, "z")
            .is_ok());
    }
}
