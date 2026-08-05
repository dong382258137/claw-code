#![allow(clippy::result_large_err)]
mod cache_break_detection;
mod client;
mod error;
mod http_client;
mod providers;
mod sse;
mod types;

pub use cache_break_detection::{
    cache_break_root, CacheBreakConfig, CacheBreakDetector, CacheBreakEvent, CacheBreakPaths,
    CacheBreakReasons, CacheBreakRecord, CacheBreakStats,
};
pub use client::{read_base_url, MessageStream, ProviderClient};
pub use error::{ApiError, TypedErrorEnvelope, TypedErrorPayload};
pub use http_client::{
    build_http_client, build_http_client_or_default, build_http_client_with,
    build_http_client_with_opts, ProxyConfig, TimeoutConfig,
};
pub use providers::model_tier::{
    model_meets_complexity, tier_for_model, upgrade_cost_multiplier, upgrade_model, ModelTier,
    TaskComplexity,
};
pub use providers::openai_compat::has_api_key;
pub use providers::openai_compat::{
    build_chat_completion_request, check_request_body_size, estimate_request_body_size,
    flatten_tool_result_content, is_reasoning_model, model_requires_reasoning_content_in_history,
    translate_message, OpenAiCompatClient, OpenAiCompatConfig,
};
pub use providers::{
    detect_provider_kind, max_tokens_for_model, max_tokens_for_model_with_override,
    metadata_for_model, model_family_identity_for, model_family_identity_for_kind,
    model_token_limit, provider_diagnostics_for_model, resolve_model_alias, ModelTokenLimit,
    ProviderDiagnostics, ProviderKind,
};
pub use sse::{parse_frame, SseParser};
pub use types::{
    CacheControl, ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent,
    ContentBlockStopEvent, InputContentBlock, InputMessage, MessageDelta, MessageDeltaEvent,
    MessageRequest, MessageResponse, MessageStartEvent, MessageStopEvent, OutputContentBlock,
    StreamEvent, SystemBlock, SystemContent, ToolChoice, ToolDefinition, ToolResultContentBlock,
    Usage,
};

pub use telemetry::{
    AnalyticsEvent, ClientIdentity, JsonlTelemetrySink, MemoryTelemetrySink, SessionTraceRecord,
    SessionTracer, TelemetryEvent, TelemetrySink,
};
