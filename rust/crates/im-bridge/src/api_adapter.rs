//! Thin `ApiClient` adapter that wraps `api::ProviderClient`.
//!
//! Converts `runtime::ApiRequest` → `api::MessageRequest`, calls the
//! provider, and converts the response back to `Vec<runtime::AssistantEvent>`.
//!
//! ## P0-1 fix: no runtime nesting
//!
//! `stream()` is called from inside the agent's `current_thread` runtime +
//! LocalSet. Creating another runtime and calling `block_on` would panic.
//! Instead, we spawn a **dedicated OS thread** for each HTTP call, which
//! creates its own one-shot runtime. This avoids "Cannot start a runtime
//! from within a runtime" panics.
//!
//! ## P1-6 fix: tool definitions
//!
//! Holds a `GlobalToolRegistry` (from the `tools` crate) and injects real
//! `ToolDefinition`s into every `MessageRequest`, so the LLM knows what tools
//! are available.

use api::{MessageRequest, ProviderClient, SystemBlock, SystemContent};
use runtime::{
    ApiClient as RuntimeApiClient, ApiRequest, AssistantEvent, ConversationMessage, MessageRole,
    RuntimeError, TokenUsage,
};
use tools::GlobalToolRegistry;

/// An `ApiClient` implementation that delegates to `api::ProviderClient`.
///
/// `Clone` so each per-session agent can have its own copy (隐患-12 fix).
#[derive(Clone)]
pub struct BridgeApiClient {
    client: ProviderClient,
    model: String,
    /// Whether to include tool definitions.
    enable_tools: bool,
    /// Maximum output tokens.
    max_tokens: u32,
    /// Tool registry for generating `ToolDefinition`s.
    tool_registry: GlobalToolRegistry,
}

impl BridgeApiClient {
    pub fn new(
        client: ProviderClient,
        model: String,
        enable_tools: bool,
        tool_registry: GlobalToolRegistry,
    ) -> Self {
        Self {
            client,
            model,
            enable_tools,
            max_tokens: 4096,
            tool_registry,
        }
    }

    #[allow(dead_code)]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

impl RuntimeApiClient for BridgeApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let system = Self::build_system(&request.system_prompt);
        let messages = Self::convert_messages(&request.messages);
        let tools = if self.enable_tools {
            Some(self.tool_registry.definitions(None))
        } else {
            None
        };

        let msg_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            messages,
            system,
            tools,
            tool_choice: None,
            stream: false,
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            reasoning_effort: None,
            extra_body: Default::default(),
        };

        // P0-1 fix: spawn a dedicated OS thread with its own runtime.
        // The agent's `stream()` is called from within a `current_thread`
        // runtime + LocalSet. Creating another runtime and calling `block_on`
        // here would panic with "Cannot start a runtime from within a runtime".
        //
        // By spawning a fresh thread, we get an isolated runtime context.
        // `ProviderClient` is `Clone + Send`, so we clone it for the thread.
        let client = self.client.clone();

        let (tx, rx) = std::sync::mpsc::channel::<Result<api::MessageResponse, String>>();

        std::thread::Builder::new()
            .name("im-bridge-api".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let rt = match rt {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(format!("runtime build failed: {e}")));
                        return;
                    }
                };
                let result = rt.block_on(async move {
                    client.send_message(&msg_request).await
                });
                let _ = tx.send(result.map_err(|e| e.to_string()));
            })
            .map_err(|e| RuntimeError::new(format!("failed to spawn API thread: {e}")))?;

        let result = rx
            .recv()
            .map_err(|e| RuntimeError::new(format!("API thread channel error: {e}")))?;

        let response = result
            .map_err(|e| RuntimeError::new(format!("API error: {e}")))?;

        Ok(Self::convert_response(&response))
    }
}

impl BridgeApiClient {
    fn build_system(split: &runtime::SystemPromptSplit) -> Option<SystemContent> {
        let mut blocks: Vec<SystemBlock> = Vec::new();
        for section in &split.static_sections {
            if !section.is_empty() {
                blocks.push(SystemBlock::new(section.clone()));
            }
        }
        for section in &split.dynamic_sections {
            if !section.is_empty() {
                blocks.push(SystemBlock::new(section.clone()));
            }
        }
        if blocks.is_empty() {
            None
        } else {
            Some(SystemContent::from_blocks(blocks))
        }
    }

    fn convert_messages(msgs: &[ConversationMessage]) -> Vec<api::InputMessage> {
        msgs.iter()
            .map(|msg| {
                let role = match msg.role {
                    MessageRole::User => "user".to_string(),
                    MessageRole::Assistant => "assistant".to_string(),
                    MessageRole::System => "user".to_string(), // Map System to user (shouldn't happen in practice)
                    MessageRole::Tool => "user".to_string(),
                };

                let content = msg
                    .blocks
                    .iter()
                    .map(|block| match block {
                        runtime::ContentBlock::Text { text } => {
                            api::InputContentBlock::Text { text: text.clone() }
                        }
                        runtime::ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => api::InputContentBlock::Thinking {
                            thinking: thinking.clone(),
                            signature: signature.clone(),
                        },
                        runtime::ContentBlock::ToolUse { id, name, input } => {
                            api::InputContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: serde_json::from_str(input).unwrap_or_default(),
                            }
                        }
                        runtime::ContentBlock::ToolResult {
                            tool_use_id,
                            tool_name: _,
                            output,
                            is_error,
                        } => api::InputContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: vec![api::ToolResultContentBlock::Text {
                                text: output.clone(),
                            }],
                            is_error: *is_error,
                        },
                    })
                    .collect();

                api::InputMessage { role, content }
            })
            .collect()
    }

    fn convert_response(response: &api::MessageResponse) -> Vec<AssistantEvent> {
        let mut events: Vec<AssistantEvent> = Vec::new();

        for block in &response.content {
            match block {
                api::OutputContentBlock::Text { text } => {
                    events.push(AssistantEvent::TextDelta(text.clone()));
                }
                api::OutputContentBlock::ToolUse { id, name, input } => {
                    events.push(AssistantEvent::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::to_string(input).unwrap_or_default(),
                    });
                }
                api::OutputContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    events.push(AssistantEvent::Thinking {
                        thinking: thinking.clone(),
                        signature: signature.clone(),
                    });
                }
                _ => {}
            }
        }

        // Convert usage
        let usage = &response.usage;
        events.push(AssistantEvent::Usage(TokenUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        }));

        events.push(AssistantEvent::MessageStop);
        events
    }
}
