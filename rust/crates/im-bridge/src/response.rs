//! Response collector: listens for agent notifications, collects chunks,
//! and dispatches complete responses back to IM platforms.
//!
//! Also handles direct command responses (commands bypass the agent).

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol as acp;
use claw_acp::AcpClientMessage;
use tokio::sync::mpsc;

use crate::connectors::feishu::FeishuClient;
use crate::connectors::wecom::WeComClient;

/// Maps ACP session_id → route target for response dispatch.
pub type SessionRouter = Arc<tokio::sync::Mutex<HashMap<acp::SessionId, RouteTarget>>>;

/// Where to send the completed response.
#[derive(Clone)]
pub enum RouteTarget {
    Feishu {
        client: FeishuClient,
        chat_id: String,
    },
    WeCom {
        client: WeComClient,
        chat_id: String,
    },
}

/// Signal that a prompt has completed.
#[derive(Debug)]
pub struct PromptCompleted {
    pub session_id: acp::SessionId,
    /// Whether the prompt succeeded (even if empty response).
    pub success: bool,
}

/// Collects agent notifications and dispatches complete responses.
pub struct ResponseCollector {
    notification_rx: mpsc::UnboundedReceiver<AcpClientMessage>,
    completion_rx: mpsc::UnboundedReceiver<PromptCompleted>,
    router: SessionRouter,
    partials: HashMap<acp::SessionId, String>,
}

impl ResponseCollector {
    pub fn new(
        notification_rx: mpsc::UnboundedReceiver<AcpClientMessage>,
        completion_rx: mpsc::UnboundedReceiver<PromptCompleted>,
        router: SessionRouter,
    ) -> Self {
        Self {
            notification_rx,
            completion_rx,
            router,
            partials: HashMap::new(),
        }
    }

    /// Run the collection loop.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                // Notification from agent (text chunks)
                msg = self.notification_rx.recv() => {
                    match msg {
                        Some(AcpClientMessage::SessionNotification(args)) => {
                            if let acp::SessionUpdate::AgentMessageChunk(chunk) = &args.request.update {
                                if let acp::ContentBlock::Text(text) = &chunk.content {
                                    self.partials
                                        .entry(args.request.session_id.clone())
                                        .or_default()
                                        .push_str(&text.text);
                                }
                            }
                        }
                        Some(_) => {} // Ignore other client messages
                        None => break, // Channel closed
                    }
                }
                // Prompt completed signal
                msg = self.completion_rx.recv() => {
                    match msg {
                        Some(PromptCompleted { session_id, .. }) => {
                            self.dispatch_if_complete(&session_id).await;
                        }
                        None => break,
                    }
                }
            }
        }
        tracing::info!("response collector: exiting");
    }

    async fn dispatch_if_complete(&mut self, session_id: &acp::SessionId) {
        if let Some(text) = self.partials.remove(session_id) {
            let text = text.trim().to_string();
            if text.is_empty() {
                return;
            }

            let router = self.router.lock().await;
            if let Some(target) = router.get(session_id) {
                match target {
                    RouteTarget::Feishu { client, chat_id } => {
                        if let Err(e) = client.send_text_message(chat_id, &text).await {
                            tracing::error!(
                                "failed to send feishu response for session {}: {}",
                                session_id,
                                e
                            );
                        }
                    }
                    RouteTarget::WeCom { client, chat_id } => {
                        if let Err(e) = client.push_text_message(chat_id, &text).await {
                            tracing::error!(
                                "failed to send wecom response for session {}: {}",
                                session_id,
                                e
                            );
                        }
                    }
                }
            } else {
                tracing::warn!(
                    "no route target for session {}, response dropped ({} chars)",
                    session_id,
                    text.len()
                );
            }
        }
    }
}
