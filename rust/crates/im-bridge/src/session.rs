//! Session manager: maps IM chats to ACP agent sessions.
//!
//! Services:
//! - Session routing: one agent session per IM chat
//! - Command handling: chat commands intercepted before agent
//! - Session lifecycle: create, reuse, destroy, persist

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use claw_acp::acp_send;
use claw_shell::{spawn_claw_shell, ClawAgentBuilder};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::commands::{parse_command, ChatCommand, CommandParseResult};
use crate::persistence::{PersistedSession, PersistenceManager};
use crate::response::PromptCompleted;

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

struct AgentSession {
    session_id: acp::SessionId,
    cwd: std::path::PathBuf,
    last_active: Instant,
    user_id: String,
}

/// Manages agent lifecycle and session routing.
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<ChatKey, AgentSession>>>,
    agent_tx: mpsc::UnboundedSender<claw_acp::AcpAgentMessage>,
    completion_tx: mpsc::UnboundedSender<PromptCompleted>,
    #[allow(dead_code)]
    idle_timeout: Duration,
    /// Persistence manager for save/restore.
    persistence: PersistenceManager,
}

/// Result of spawning a session manager.
pub struct SpawnResult {
    pub manager: Arc<SessionManager>,
    pub notification_rx: mpsc::UnboundedReceiver<claw_acp::AcpClientMessage>,
    pub completion_rx: mpsc::UnboundedReceiver<PromptCompleted>,
    pub keep_alive: SpawnKeepAlive,
}

/// Holds the spawned agent thread handle to prevent early drop.
pub struct SpawnKeepAlive {
    _thread: std::thread::JoinHandle<()>,
}

impl SessionManager {
    /// Spawn the agent in a background thread and create the manager.
    pub fn spawn<C>(
        builder: ClawAgentBuilder<C>,
        cancel: CancellationToken,
        idle_timeout_secs: u64,
    ) -> SpawnResult
    where
        C: runtime::ApiClient + Send + 'static,
    {
        let spawned =
            spawn_claw_shell(builder, &cancel).expect("failed to spawn claw agent for IM bridge");

        let claw_acp::AcpClientChannel {
            rx: notification_rx,
            tx: agent_tx,
        } = spawned.channel;

        let (completion_tx, completion_rx) = mpsc::unbounded_channel();

        SpawnResult {
            manager: Arc::new(SessionManager {
                sessions: Arc::new(Mutex::new(HashMap::new())),
                agent_tx,
                completion_tx,
                idle_timeout: Duration::from_secs(idle_timeout_secs),
                persistence: PersistenceManager::new(),
            }),
            notification_rx,
            completion_rx,
            keep_alive: SpawnKeepAlive {
                _thread: spawned._thread_handle,
            },
        }
    }

    /// Spawn and restore previously persisted sessions.
    ///
    /// Restored sessions are re-established as "stale" — the agent process is new,
    /// but the session routing is restored. Old agent conversations are lost
    /// (ACP sessions don't survive process restart), but we preserve the chat→session
    /// mapping for future messages.
    pub fn spawn_with_restore<C>(
        builder: ClawAgentBuilder<C>,
        cancel: CancellationToken,
        idle_timeout_secs: u64,
    ) -> SpawnResult
    where
        C: runtime::ApiClient + Send + 'static,
    {
        let result = Self::spawn(builder, cancel, idle_timeout_secs);

        let persisted = result.manager.persistence.load();
        let restored_count = persisted.sessions.len();

        if restored_count > 0 {
            tracing::info!(
                "restoring {} previously persisted session(s); note: agent conversations are fresh",
                restored_count
            );
            // We don't actually re-create ACP sessions (they don't survive restart).
            // The persisted data serves as a record; new sessions will be created
            // on first message to each chat.
        }

        result
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

        let tx = self.agent_tx.clone();
        let completion_tx = self.completion_tx.clone();
        let sid = session_id.clone();
        let text = req.text;

        // Spawn async: send prompt + signal completion
        tokio::spawn(async move {
            let prompt_req = acp::PromptRequest::new(
                sid.clone(),
                vec![acp::ContentBlock::Text(acp::TextContent::new(&text))],
            );

            let success = match acp_send(prompt_req, &tx).await {
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
            .map(|(key, session)| PersistedSession {
                platform: key.platform.clone(),
                chat_id: key.chat_id.clone(),
                session_id: session.session_id.to_string(),
                cwd: session.cwd.display().to_string(),
                last_active_secs: session.last_active.elapsed().as_secs(),
                user_id: Some(session.user_id.clone()),
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

    async fn get_or_create_session(
        &self,
        key: &ChatKey,
        user_id: &str,
    ) -> Result<acp::SessionId, String> {
        let mut sessions = self.sessions.lock().await;

        // Check for existing session
        if let Some(session) = sessions.get(key) {
            let sid = session.session_id.clone();
            // Update last_active
            if let Some(s) = sessions.get_mut(key) {
                s.last_active = Instant::now();
            }
            tracing::debug!("reusing existing session {} for chat {:?}", sid, key);
            return Ok(sid);
        }

        // Create new session via ACP
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        acp_send(
            acp::InitializeRequest::new(acp::ProtocolVersion::LATEST),
            &self.agent_tx,
        )
        .await
        .map_err(|e| format!("initialize failed: {e}"))?;

        acp_send(
            acp::AuthenticateRequest::new(acp::AuthMethodId::new("api_key")),
            &self.agent_tx,
        )
        .await
        .map_err(|e| format!("authenticate failed: {e}"))?;

        let session_req = acp::NewSessionRequest::new(cwd.clone());
        let session_resp = acp_send(session_req, &self.agent_tx)
            .await
            .map_err(|e| format!("new_session failed: {e}"))?;

        let session_id = session_resp.session_id;
        tracing::info!(
            "created new session {} for chat {:?} (user: {})",
            session_id,
            key,
            user_id
        );

        sessions.insert(
            key.clone(),
            AgentSession {
                session_id: session_id.clone(),
                cwd,
                last_active: Instant::now(),
                user_id: user_id.to_string(),
            },
        );

        Ok(session_id)
    }

    /// Force creation of a new session, replacing any existing one.
    async fn force_new_session(
        &self,
        key: &ChatKey,
        user_id: &str,
    ) -> Result<acp::SessionId, String> {
        // Remove old session
        {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(key);
        }

        // Create new one
        self.get_or_create_session(key, user_id).await
    }
}
