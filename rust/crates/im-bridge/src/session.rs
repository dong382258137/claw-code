//! Session manager: maps IM chats to per-chat ACP agent sessions.
//!
//! Services:
//! - Session routing: one agent session per IM chat
//! - Command handling: chat commands intercepted before agent
//! - Session lifecycle: create, reuse, destroy, persist
//!
//! ## Architecture (multi-agent, fixes 隐患-12)
//!
//! Each `ChatKey` gets its own `spawn_claw_shell` agent thread, so concurrent
//! users on different chats don't block each other. All agents' notification
//! streams are merged into one channel for `ResponseCollector`.
//!
//! ## P0-2 fix: lock-free ACP handshake
//!
//! `get_or_create_session` does NOT hold the sessions Mutex across `await`
//! points. It locks briefly to check, drops the lock for the ACP handshake,
//! then re-locks to insert — handling the race with double-checked insertion.
//!
//! ## P2-7: idle timeout cleanup
//!
//! A background task periodically scans for sessions idle longer than
//! `idle_timeout` and cancels their agent threads.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use claw_acp::{acp_send, AcpClientMessage};
use claw_shell::{spawn_claw_shell, ClawAgentBuilder};
use runtime::{
    bus_now_ms, global_session_bus, BusMessage, BusMessageKind, BusPeer, PeerKind, PeerStatus,
    PermissionPolicy,
};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::api_adapter::BridgeApiClient;
use crate::commands::{parse_command, ChatCommand, CommandParseResult};
use crate::persistence::{PersistedSession, PersistenceManager};
use crate::response::{PromptCompleted, RouteTarget};
use crate::tools::register_default_tools;

/// Identifies a specific IM chat across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatKey {
    pub platform: String,
    pub chat_id: String,
}

impl ChatKey {
    pub fn new(platform: impl Into<String>, chat_id: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
            chat_id: chat_id.into(),
        }
    }

    /// Parse from a "platform:chat_id" string (persistence format).
    #[allow(dead_code)]
    pub fn from_persisted(s: &str) -> Option<Self> {
        let (platform, chat_id) = s.split_once(':')?;
        Some(Self {
            platform: platform.to_string(),
            chat_id: chat_id.to_string(),
        })
    }

    /// 本频道在 SessionBus 中的 peer id：`im:{platform}:{chat_id}`。
    ///
    /// 跨频道目标也以此格式书写（如 `/bus send im:feishu:oc_123 你好`）。
    pub fn bus_peer_id(&self) -> String {
        format!("im:{}:{}", self.platform, self.chat_id)
    }
}

/// A user message to be processed by the agent.
#[derive(Debug, Clone)]
pub struct ImRequest {
    pub chat_key: ChatKey,
    pub user_id: String,
    pub text: String,
}

/// Per-chat agent entry. Each chat gets its own agent thread + ACP channels.
struct AgentEntry {
    /// Sender for ACP requests (Prompt, Cancel, etc.) → agent thread.
    agent_tx: mpsc::UnboundedSender<claw_acp::AcpAgentMessage>,
    session_id: acp::SessionId,
    cwd: std::path::PathBuf,
    last_active: Instant,
    user_id: String,
    /// Cancel token for this agent's thread. Firing it causes the agent to exit.
    cancel: CancellationToken,
}

/// Manages agent lifecycle and session routing.
///
/// Multi-agent design (隐患-12 fix): each `ChatKey` has its own agent thread,
/// so concurrent chats are processed in parallel rather than serialized through
/// a single agent.
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<ChatKey, AgentEntry>>>,
    /// Template client — cloned for each new agent.
    api_client: BridgeApiClient,
    system_prompt: Vec<String>,
    permission_policy: PermissionPolicy,
    parent_cancel: CancellationToken,
    /// Merged notification channel: all agents forward their `AcpClientMessage`s here.
    notification_tx: mpsc::UnboundedSender<AcpClientMessage>,
    completion_tx: mpsc::UnboundedSender<PromptCompleted>,
    idle_timeout: Duration,
    persistence: PersistenceManager,
    /// 上次进程遗留的持久化元数据(load 自磁盘)。persist 时与当前活跃 session
    /// 合并,防止进程重启后第一轮 persist 用空内存 map 覆盖历史记录。
    persisted_metadata: std::sync::Mutex<Vec<PersistedSession>>,
    /// Session Bus（Epic 3 IM hub）：跨频道互通路由表 `im:{platform}:{chat_id}` → 回复目标。
    /// 供 `/bus send` 向本地 IM 频道直发（unread 之外的真实 IM 消息）。
    bus_router: std::sync::Mutex<HashMap<String, RouteTarget>>,
}

/// Result of spawning a session manager.
pub struct SpawnResult {
    pub manager: Arc<SessionManager>,
    pub notification_rx: mpsc::UnboundedReceiver<AcpClientMessage>,
    pub completion_rx: mpsc::UnboundedReceiver<PromptCompleted>,
    pub keep_alive: SpawnKeepAlive,
}

/// Holds background task handles to prevent early drop.
pub struct SpawnKeepAlive {
    /// Idle-cleanup task handle.
    _idle_task: Option<tokio::task::JoinHandle<()>>,
}

impl SessionManager {
    /// Spawn the session manager with multi-agent support.
    ///
    /// Each new chat session will spawn its own `ClawAgentBuilder` (with a cloned
    /// `BridgeApiClient`) and run in a dedicated thread, so concurrent users
    /// don't block each other.
    ///
    /// Persisted sessions from a previous run are loaded for awareness; new
    /// agent sessions are created on first message to each chat (ACP sessions
    /// don't survive process restart).
    pub fn spawn(
        api_client: BridgeApiClient,
        system_prompt: Vec<String>,
        permission_policy: PermissionPolicy,
        parent_cancel: CancellationToken,
        idle_timeout_secs: u64,
    ) -> SpawnResult {
        let (notification_tx, notification_rx) = mpsc::unbounded_channel();
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();

        let manager = Arc::new(SessionManager {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            api_client,
            system_prompt,
            permission_policy,
            parent_cancel: parent_cancel.clone(),
            notification_tx,
            completion_tx,
            idle_timeout: Duration::from_secs(idle_timeout_secs),
            persistence: PersistenceManager::new(),
            persisted_metadata: std::sync::Mutex::new(Vec::new()),
            bus_router: std::sync::Mutex::new(HashMap::new()),
        });

        // P2-7: Spawn idle-cleanup background task
        let cleanup_mgr = manager.clone();
        let idle_task = tokio::spawn(async move {
            cleanup_mgr.run_idle_cleanup().await;
        });

        // 隐患-11: Load persisted sessions for awareness only.
        // ACP sessions don't survive restart, so we can't truly "restore" them.
        // New agent sessions are created on first message to each chat.
        // 同时把元数据回灌 persisted_metadata,persist 时合并,避免覆盖清空历史。
        let persisted = manager.persistence.load();
        let persisted_count = persisted.sessions.len();
        if persisted_count > 0 {
            *manager
                .persisted_metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = persisted.sessions;
            tracing::info!(
                "loaded {} persisted session record(s); new agents will be created on first message",
                persisted_count
            );
        }

        SpawnResult {
            manager,
            notification_rx,
            completion_rx,
            keep_alive: SpawnKeepAlive {
                _idle_task: Some(idle_task),
            },
        }
    }

    /// Get the persistence manager reference (for periodic saves).
    pub fn persistence(&self) -> &PersistenceManager {
        &self.persistence
    }

    /// Get the total number of active sessions (for /status).
    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Handle a chat command, returning the response if it was a command.
    ///
    /// Returns:
    /// - `Ok(Some(response))` — recognized command, reply with this directly
    /// - `Ok(None)` — not a command, proceed to agent
    /// - `Err(msg)` — command processing failed
    pub async fn handle_command(&self, req: &ImRequest) -> Result<Option<String>, String> {
        match parse_command(&req.text) {
            CommandParseResult::Command(cmd) => {
                let response = match &cmd {
                    ChatCommand::Bus { args } => self.handle_bus_command(&req.chat_key, args).await,
                    ChatCommand::NewSession => {
                        // Force a new session creation
                        self.force_new_session(&req.chat_key, &req.user_id).await?;
                        "✅ Started a new session. Your conversation history has been cleared."
                            .to_string()
                    }
                    ChatCommand::Help | ChatCommand::Status | ChatCommand::History => {
                        let session_id = {
                            let sessions = self.sessions.lock().await;
                            sessions
                                .get(&req.chat_key)
                                .map(|s| s.session_id.clone())
                                .unwrap_or_else(|| acp::SessionId::new("no-active-session"))
                        };
                        let count = self.session_count().await;
                        crate::commands::handle_command(&cmd, &session_id, count).await
                    }
                };
                Ok(Some(response))
            }
            CommandParseResult::Message(_) => Ok(None),
        }
    }

    /// 注册 IM 频道的 bus 直发路由：`im:{platform}:{chat_id}` → RouteTarget。
    ///
    /// server.rs 在建立 session_id → RouteTarget 映射的同时调用本方法，使
    /// `/bus send` 能向本地已注册的 IM 频道直发真实 IM 消息（跨频道协作）。
    pub async fn register_bus_route(&self, chat_key: &ChatKey, target: RouteTarget) {
        self.bus_router
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(chat_key.bus_peer_id(), target);
    }

    /// 执行 `/bus` 子命令：list / send / watch / unwatch（Epic 3 IM hub）。
    ///
    /// - `list`：列出全部 peer（含跨进程 remote peer）。
    /// - `send <target> <text>`：publish 到总线；若目标是本 hub 已注册的 IM 频道，
    ///   同时通过 RouteTarget 直发真实 IM 消息。
    /// - `watch/unwatch <target>`：订阅/取消订阅目标 peer 的消息流。
    async fn handle_bus_command(&self, chat_key: &ChatKey, args: &str) -> String {
        let bus = global_session_bus();
        let own_id = chat_key.bus_peer_id();
        let mut parts = args.split_whitespace();
        match parts.next() {
            None | Some("") => {
                crate::commands::handle_command(
                    &ChatCommand::Bus {
                        args: String::new(),
                    },
                    &acp::SessionId::new("bus"),
                    0,
                )
                .await
            }
            Some("list") => {
                let mut lines = vec![format!("**Session Bus Peers**（本频道 `{own_id}`）")];
                let mut peers = bus.peers_snapshot();
                // 追加跨进程邮箱发现的远端 peer（.claw/bus/ 文件事件队列）
                if let Some(root) = bus.bus_root() {
                    peers.extend(bus.remote_peers(&root, &own_id));
                }
                if peers.is_empty() {
                    lines.push("(no peers yet — 任一频道首次发消息后出现)".to_string());
                }
                for p in &peers {
                    lines.push(format!(
                        "- `{}` · {} · {} · unread {}",
                        p.session_id,
                        p.kind.as_str(),
                        p.status.as_str(),
                        p.unread
                    ));
                }
                lines.join("\n")
            }
            Some("send") => {
                let target = match parts.next() {
                    Some(t) => t.to_string(),
                    None => {
                        return "Usage: `/bus send <target> <text>`".to_string();
                    }
                };
                let text = parts.collect::<Vec<_>>().join(" ");
                if text.is_empty() {
                    return "Usage: `/bus send <target> <text>`".to_string();
                }
                self.format_bus_send_result(&own_id, &target, &text).await
            }
            Some("watch") => {
                let Some(target) = parts.next() else {
                    return "Usage: `/bus watch <target>`".to_string();
                };
                match bus.watch(&own_id, target) {
                    Ok(()) => format!("👁 已订阅 `{target}` 的消息流"),
                    Err(e) => format!("❌ {e}"),
                }
            }
            Some("unwatch") => {
                let Some(target) = parts.next() else {
                    return "Usage: `/bus unwatch <target>`".to_string();
                };
                bus.unwatch(&own_id, target);
                format!("已取消订阅 `{target}`")
            }
            Some(other) => format!(
                "Unknown /bus subcommand: `{other}`\n\n{}",
                crate::commands::handle_command(
                    &ChatCommand::Bus {
                        args: String::new()
                    },
                    &acp::SessionId::new("bus"),
                    0,
                )
                .await
            ),
        }
    }

    /// 总线发送 + IM 直发（Epic 3 hub 核心，供 `/bus send` 与 `POST /api/bus/send` 复用）。
    ///
    /// 1. 构造 `Message` 消息并 `publish` 到全局总线（`to` 未注册时仅写入 unread/文件队列）。
    /// 2. 若目标是本 hub 已注册的 IM 频道（`bus_router` 命中），通过 RouteTarget
    ///    向目标聊天直发真实 IM 消息（跨频道协作的用户可见效果）。
    ///
    /// 返回 `(delivered, pushed_im)`——送达 peer 列表 + 是否已推送真实 IM 消息。
    pub async fn bus_send_and_push(
        &self,
        from: &str,
        to: &str,
        text: &str,
    ) -> Result<(Vec<String>, bool), String> {
        let bus = global_session_bus();
        let msg = BusMessage {
            from: from.to_string(),
            to: to.to_string(),
            kind: BusMessageKind::Message,
            payload: serde_json::json!({ "text": text }),
            hop: 0,
            ts_ms: bus_now_ms(),
        };
        let delivered = bus.publish(msg)?;
        // 目标为本 hub 已注册的 IM 频道 → 直发真实 IM 消息
        let pushed_im = if to.starts_with("im:") {
            self.push_im_route(to, text, from).await
        } else {
            false
        };
        Ok((delivered, pushed_im))
    }

    /// 查询 `im:{platform}:{chat_id}` 是否已注册本 hub 直发路由。
    pub fn bus_route_for(&self, peer_id: &str) -> bool {
        self.bus_router
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(peer_id)
    }

    /// 向已注册的 IM 频道直发真实 IM 消息（跨频道协作的用户可见效果）。
    /// 返回是否成功推送。无路由或发送失败返回 `false`。
    pub async fn push_im_route(&self, peer_id: &str, text: &str, from: &str) -> bool {
        let route = self
            .bus_router
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(peer_id)
            .cloned();
        let Some(rt) = route else {
            return false;
        };
        let display = format!("📨 来自 `{from}`:\n{text}");
        match rt {
            RouteTarget::Feishu { client, chat_id } => {
                client.send_text_message(&chat_id, &display).await.is_ok()
            }
            RouteTarget::WeCom { client, chat_id } => {
                client.push_text_message(&chat_id, &display).await.is_ok()
            }
        }
    }

    /// 当前已注册的所有 IM 频道 key（供 bus 邮箱轮询遍历，审查补充 2026-08-12）。
    pub async fn registered_chats(&self) -> Vec<ChatKey> {
        self.sessions.lock().await.keys().cloned().collect()
    }

    /// 启动 Session Bus 邮箱轮询（审查补充 2026-08-12）。
    ///
    /// 周期性消费各已注册 IM 频道在 `bus_root` 下的邮箱：把远端进程（TUI 主会话 /
    /// IDE 面板）经文件事件队列投递的消息注入本地总线；若目标是本 hub 的 IM 频道，
    /// 同时直发真实 IM 消息（跨进程广播 `*` / 定向投递的用户可见效果）。
    pub fn start_bus_mailbox_poller(
        self: &Arc<Self>,
        bus_root: std::path::PathBuf,
    ) -> tokio::task::JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tick.tick().await;
                let chats = manager.registered_chats().await;
                if chats.is_empty() {
                    continue;
                }
                let bus = global_session_bus();
                for key in chats {
                    let peer_id = key.bus_peer_id();
                    let msgs = runtime::SessionBus::consume_mailbox(&bus_root, &peer_id);
                    if msgs.is_empty() {
                        continue;
                    }
                    for msg in msgs {
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
                            kind: PeerKind::Ide,
                            status: PeerStatus::Idle,
                            unread: 0,
                            last_seen_ms: bus_now_ms(),
                            config_path: Some(runtime::SessionBus::mailbox_dir(
                                &bus_root, &msg.from,
                            )),
                        });
                        // 注入（跳过接收端权限重检：发送端已校验）
                        let _ = bus.inject(msg.clone());
                        // 目标是本 hub IM 频道 → 直发真实 IM 消息
                        if msg.to.starts_with("im:") {
                            let _ = manager
                                .push_im_route(&msg.to, msg.text_payload(), &msg.from)
                                .await;
                        }
                    }
                }
            }
        })
    }

    /// `/bus send` 的用户可读结果（内部格式化 `bus_send_and_push` 返回值）。
    async fn format_bus_send_result(&self, from: &str, to: &str, text: &str) -> String {
        match self.bus_send_and_push(from, to, text).await {
            Ok((delivered, pushed_im)) => {
                let mut msg_txt = format!("✅ 已发送到 `{to}`（总线投递 {}）", delivered.len());
                if pushed_im {
                    msg_txt.push_str("，并已推送到目标 IM 频道");
                } else if delivered.is_empty() {
                    // 审查补充(2026-08-12):delivered 空 = 目标未注册/权限拒绝/文件
                    // 路由落盘,不再用"目标未注册,仅写入总线"误导(可能实际被权限拒绝)。
                    msg_txt.push_str("。目标不可达（未注册或权限拒绝），已尝试写入总线");
                }
                msg_txt
            }
            Err(e) => format!("❌ {e}"),
        }
    }

    /// Process an incoming IM request asynchronously.
    ///
    /// Returns immediately; the response will be dispatched via the completion channel.
    pub async fn process_request(&self, req: ImRequest) -> Result<acp::SessionId, String> {
        let session_id = self
            .get_or_create_session(&req.chat_key, &req.user_id)
            .await?;
        // Get the agent_tx for this chat (clone the sender, don't hold the lock)
        let agent_tx = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(&req.chat_key)
                .map(|e| e.agent_tx.clone())
                .ok_or_else(|| "session disappeared during process_request".to_string())?
        };

        // Update last_active
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(entry) = sessions.get_mut(&req.chat_key) {
                entry.last_active = Instant::now();
            }
        }

        let completion_tx = self.completion_tx.clone();
        let sid = session_id.clone();
        let text = req.text;

        // Spawn async: send prompt + signal completion
        tokio::spawn(async move {
            let prompt_req = acp::PromptRequest::new(
                sid.clone(),
                vec![acp::ContentBlock::Text(acp::TextContent::new(&text))],
            );

            let success = match acp_send(prompt_req, &agent_tx).await {
                Ok(_) => {
                    tracing::info!("prompt sent for session {}", sid);
                    true
                }
                Err(e) => {
                    tracing::error!("prompt send failed for session {}: {e}", sid);
                    false
                }
            };

            let _ = completion_tx.send(PromptCompleted {
                session_id: sid.clone(),
                success,
            });

            if !success {
                tracing::error!(
                    "session {}: agent may have crashed, consider restarting",
                    sid
                );
            }
        });

        Ok(session_id)
    }

    /// Collect all active sessions for persistence.
    pub async fn collect_persistable_sessions(&self) -> Vec<PersistedSession> {
        let sessions = self.sessions.lock().await;
        sessions
            .iter()
            .map(|(key, entry)| PersistedSession {
                platform: key.platform.clone(),
                chat_id: key.chat_id.clone(),
                session_id: entry.session_id.to_string(),
                cwd: entry.cwd.display().to_string(),
                last_active_secs: entry.last_active.elapsed().as_secs(),
                user_id: Some(entry.user_id.clone()),
            })
            .collect()
    }

    /// Save current sessions to disk.
    pub async fn persist_sessions(&self) {
        let sessions = self.collect_persistable_sessions().await;
        let merged = self.merge_with_history(sessions);
        let data = crate::persistence::PersistenceData {
            sessions: merged,
            schema_version: 1,
        };
        if let Err(e) = self.persistence.save(&data) {
            tracing::error!("failed to persist sessions: {e}");
        }
    }

    /// 合并历史元数据与当前活跃 session,按 (platform, chat_id) 去重,活跃优先。
    /// 防止进程重启后第一轮 persist 用空内存 map 覆盖磁盘上的历史记录。
    fn merge_with_history(&self, active: Vec<PersistedSession>) -> Vec<PersistedSession> {
        let merged = {
            let history = self
                .persisted_metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            Self::merge_sessions(&history, &active)
        };
        // 更新历史缓存,下一轮 persist 以本轮为准(避免过期历史反复合入)。
        *self
            .persisted_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = merged.clone();
        merged
    }

    /// 纯函数:按 (platform, chat_id) 合并历史与活跃记录,活跃覆盖历史。
    fn merge_sessions(
        history: &[PersistedSession],
        active: &[PersistedSession],
    ) -> Vec<PersistedSession> {
        let mut merged: HashMap<(String, String), PersistedSession> = HashMap::new();
        for s in history {
            merged.insert((s.platform.clone(), s.chat_id.clone()), s.clone());
        }
        for s in active {
            merged.insert((s.platform.clone(), s.chat_id.clone()), s.clone());
        }
        merged.into_values().collect()
    }

    // ── private helpers ───────────────────────────────────────

    /// Get an existing session for the chat, or create a new one.
    ///
    /// P0-2 fix: the Mutex is NOT held across `await` points. The flow is:
    /// 1. Lock briefly to check for an existing session → drop lock
    /// 2. Perform ACP handshake (initialize/authenticate/new_session) without lock
    /// 3. Re-lock to insert — handle race condition (if another thread won, reuse theirs)
    async fn get_or_create_session(
        &self,
        key: &ChatKey,
        user_id: &str,
    ) -> Result<acp::SessionId, String> {
        // Fast path: check for existing session (brief lock, no await)
        {
            let sessions = self.sessions.lock().await;
            if let Some(entry) = sessions.get(key) {
                tracing::debug!(
                    "reusing existing session {} for chat {:?}",
                    entry.session_id,
                    key
                );
                return Ok(entry.session_id.clone());
            }
        }

        // Slow path: create a new agent + ACP handshake (NO lock held)
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        let builder = ClawAgentBuilder::new(
            self.api_client.clone(),
            self.permission_policy.clone(),
            self.system_prompt.clone(),
        )
        .with_tool_setup(|executor| {
            register_default_tools(executor);
        });

        let child_cancel = self.parent_cancel.child_token();
        let spawned =
            spawn_claw_shell(builder, &child_cancel).map_err(|e| format!("spawn agent: {e}"))?;

        let claw_acp::AcpClientChannel {
            rx: notification_rx,
            tx: agent_tx,
        } = spawned.channel;

        // Forward this agent's notifications into the merged channel
        let merged_tx = self.notification_tx.clone();
        tokio::spawn(async move {
            let mut rx = notification_rx;
            while let Some(msg) = rx.recv().await {
                if merged_tx.send(msg).is_err() {
                    break; // merged channel closed — stop forwarding
                }
            }
        });

        // ACP handshake (no sessions lock held)
        acp_send(
            acp::InitializeRequest::new(acp::ProtocolVersion::LATEST),
            &agent_tx,
        )
        .await
        .map_err(|e| format!("initialize failed: {e}"))?;

        acp_send(
            acp::AuthenticateRequest::new(acp::AuthMethodId::new("api_key")),
            &agent_tx,
        )
        .await
        .map_err(|e| format!("authenticate failed: {e}"))?;

        let session_req = acp::NewSessionRequest::new(cwd.clone());
        let session_resp = acp_send(session_req, &agent_tx)
            .await
            .map_err(|e| format!("new_session failed: {e}"))?;

        let session_id = session_resp.session_id;
        tracing::info!(
            "created new session {} for chat {:?} (user: {})",
            session_id,
            key,
            user_id
        );

        // Re-lock to insert — handle race: another thread may have inserted first
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(existing) = sessions.get(key) {
                // Race: another concurrent request created a session for this chat.
                // Cancel our agent and reuse the existing one.
                let existing_sid = existing.session_id.clone();
                tracing::debug!(
                    "race detected for chat {:?}: reusing session {}",
                    key,
                    existing_sid
                );
                drop(sessions);
                child_cancel.cancel();
                return Ok(existing_sid);
            }
            sessions.insert(
                key.clone(),
                AgentEntry {
                    agent_tx,
                    session_id: session_id.clone(),
                    cwd,
                    last_active: Instant::now(),
                    user_id: user_id.to_string(),
                    cancel: child_cancel,
                },
            );
        }

        // Epic 3：注册本频道为 Session Bus 的 `im:{platform}:{chat_id}` peer，
        // 使其他频道/外部进程可通过 `/bus send` 或 `/api/bus/send` 发现并投递。
        self.register_im_peer(key);

        Ok(session_id)
    }

    /// 注册本频道为 Session Bus 的 `Im` peer（幂等：同 id 重复注册覆盖更新）。
    fn register_im_peer(&self, key: &ChatKey) {
        let bus = global_session_bus();
        let peer_id = key.bus_peer_id();
        // 审查补充(2026-08-12):配置了 bus_root 时创建频道邮箱目录,使 TUI 主会话
        // 能经文件事件队列投递到本频道(bus 广播/定向的落点),poller 消费后直发真实 IM。
        if let Some(root) = bus.bus_root() {
            let _ = runtime::SessionBus::ensure_mailbox(&root, &peer_id);
        }
        let _ = bus.register(BusPeer {
            session_id: peer_id.clone(),
            label: format!("IM {}:{}", key.platform, key.chat_id),
            kind: PeerKind::Im,
            status: PeerStatus::Idle,
            unread: 0,
            last_seen_ms: bus_now_ms(),
            config_path: None,
        });
        tracing::debug!("registered IM bus peer {peer_id}");
    }

    /// 注销本频道 bus peer（会话被清理时调用，避免僵尸 peer）。
    fn leave_im_peer(&self, key: &ChatKey) {
        global_session_bus().leave(&key.bus_peer_id());
        self.bus_router
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key.bus_peer_id());
    }

    /// Force creation of a new session, replacing any existing one.
    ///
    /// Cancels the old agent thread and removes the entry before creating a new one.
    async fn force_new_session(
        &self,
        key: &ChatKey,
        user_id: &str,
    ) -> Result<acp::SessionId, String> {
        // Cancel and remove old agent if any
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(entry) = sessions.remove(key) {
                entry.cancel.cancel();
                tracing::info!("cancelled old agent for chat {:?}", key);
                self.leave_im_peer(key);
            }
        }

        // Create new one
        self.get_or_create_session(key, user_id).await
    }

    /// P2-7: Background task that periodically removes idle sessions.
    ///
    /// Runs every 60 seconds. Sessions whose `last_active` exceeds `idle_timeout`
    /// are cancelled (agent thread exits) and removed from the map.
    async fn run_idle_cleanup(self: Arc<Self>) {
        let check_interval = Duration::from_secs(60);
        loop {
            tokio::time::sleep(check_interval).await;

            let now = Instant::now();
            let expired: Vec<ChatKey> = {
                let sessions = self.sessions.lock().await;
                sessions
                    .iter()
                    .filter(|(_, e)| now.duration_since(e.last_active) > self.idle_timeout)
                    .map(|(k, _)| k.clone())
                    .collect()
            };

            if expired.is_empty() {
                continue;
            }

            tracing::info!(
                "idle cleanup: removing {} expired session(s)",
                expired.len()
            );
            let mut sessions = self.sessions.lock().await;
            for key in &expired {
                if let Some(entry) = sessions.remove(key) {
                    entry.cancel.cancel();
                    tracing::debug!("idle cleanup: cancelled agent for chat {:?}", key);
                    self.leave_im_peer(key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persisted_session(platform: &str, chat_id: &str, session_id: &str) -> PersistedSession {
        PersistedSession {
            platform: platform.to_string(),
            chat_id: chat_id.to_string(),
            session_id: session_id.to_string(),
            cwd: "D:\\".to_string(),
            last_active_secs: 60,
            user_id: Some("ou_user".to_string()),
        }
    }

    /// 核心回归:进程重启后活跃列表为空时,merge 必须保留历史记录,
    /// 防止 persist 用空 map 覆盖磁盘上的历史 session。
    #[test]
    fn merge_preserves_history_when_active_empty() {
        let history = vec![persisted_session("feishu", "chat_a", "sess_old")];
        let merged = SessionManager::merge_sessions(&history, &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].session_id, "sess_old");
    }

    /// 活跃记录按 (platform, chat_id) 覆盖历史记录。
    #[test]
    fn merge_active_overrides_history_same_key() {
        let history = vec![persisted_session("feishu", "chat_a", "sess_old")];
        let active = vec![persisted_session("feishu", "chat_a", "sess_new")];
        let merged = SessionManager::merge_sessions(&history, &active);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].session_id, "sess_new");
    }

    /// 活跃新增的 key 与历史 key 共存,不去重误删。
    #[test]
    fn merge_combines_distinct_keys() {
        let history = vec![persisted_session("feishu", "chat_a", "sess_old")];
        let active = vec![persisted_session("wecom", "chat_b", "sess_b")];
        let merged = SessionManager::merge_sessions(&history, &active);
        assert_eq!(merged.len(), 2);
    }

    /// Epic 3：ChatKey 的 bus peer id 格式为 `im:{platform}:{chat_id}`。
    #[test]
    fn chat_key_bus_peer_id_format() {
        let key = ChatKey::new("feishu", "oc_123456");
        assert_eq!(key.bus_peer_id(), "im:feishu:oc_123456");
        let key = ChatKey::new("wecom", "chat-abc");
        assert_eq!(key.bus_peer_id(), "im:wecom:chat-abc");
    }
}
