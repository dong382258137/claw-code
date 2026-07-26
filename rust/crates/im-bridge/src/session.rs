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
use runtime::PermissionPolicy;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::api_adapter::BridgeApiClient;
use crate::commands::{parse_command, ChatCommand, CommandParseResult};
use crate::persistence::{PersistedSession, PersistenceManager};
use crate::response::PromptCompleted;
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
        });

        // P2-7: Spawn idle-cleanup background task
        let cleanup_mgr = manager.clone();
        let idle_task = tokio::spawn(async move {
            cleanup_mgr.run_idle_cleanup().await;
        });

        // 隐患-11: Load persisted sessions for awareness only.
        // ACP sessions don't survive restart, so we can't truly "restore" them.
        // New agent sessions are created on first message to each chat.
        let persisted = manager.persistence.load();
        if !persisted.sessions.is_empty() {
            tracing::info!(
                "loaded {} persisted session record(s); new agents will be created on first message",
                persisted.sessions.len()
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
        let data = crate::persistence::PersistenceData {
            sessions,
            schema_version: 1,
        };
        if let Err(e) = self.persistence.save(&data) {
            tracing::error!("failed to persist sessions: {e}");
        }
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

        Ok(session_id)
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

            tracing::info!("idle cleanup: removing {} expired session(s)", expired.len());
            let mut sessions = self.sessions.lock().await;
            for key in &expired {
                if let Some(entry) = sessions.remove(key) {
                    entry.cancel.cancel();
                    tracing::debug!("idle cleanup: cancelled agent for chat {:?}", key);
                }
            }
        }
    }
}
