#![allow(clippy::cast_possible_truncation)]
#![allow(dead_code)]
use serde::Serialize;

use crate::error::ApiError;
use crate::types::MessageRequest;

pub mod model_tier;
pub mod openai_compat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ProviderKind {
    DeepSeek,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderMetadata {
    pub provider: ProviderKind,
    pub auth_env: &'static str,
    pub base_url_env: &'static str,
    pub default_base_url: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTokenLimit {
    pub max_output_tokens: u32,
    pub context_window_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderWireProtocol {
    OpenAiChatCompletions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFeatureSupport {
    Supported,
    Unsupported,
    PassthroughAsTool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCapabilityReport {
    pub provider: ProviderKind,
    pub wire_protocol: ProviderWireProtocol,
    pub auth_env: &'static str,
    pub base_url_env: &'static str,
    pub default_base_url: &'static str,
    pub tool_calls: ProviderFeatureSupport,
    pub streaming: ProviderFeatureSupport,
    pub streaming_usage: ProviderFeatureSupport,
    pub prompt_cache: ProviderFeatureSupport,
    pub custom_parameters: ProviderFeatureSupport,
    pub reasoning_effort: ProviderFeatureSupport,
    pub reasoning_content_history: ProviderFeatureSupport,
    pub fixed_sampling_reasoning_models: ProviderFeatureSupport,
    pub web_search: ProviderFeatureSupport,
    pub web_fetch: ProviderFeatureSupport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDiagnosticSeverity {
    Info,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderDiagnostic {
    pub code: &'static str,
    pub severity: ProviderDiagnosticSeverity,
    pub message: String,
    pub action: String,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderDiagnostics {
    pub requested_model: String,
    pub resolved_model: String,
    pub provider: ProviderKind,
    pub auth_env: &'static str,
    pub base_url_env: &'static str,
    pub default_base_url: &'static str,
    pub openai_compatible: bool,
    pub reasoning_model: bool,
    pub preserves_reasoning_content_in_history: bool,
    pub strips_tuning_params: bool,
    pub supports_stream_usage: bool,
    pub honors_proxy_env: bool,
    pub supports_extra_body_params: bool,
    pub preserves_slash_model_ids_on_custom_base_url: bool,
}

/// DeepSeek model aliases mapped to canonical names.
const MODEL_REGISTRY: &[(&str, &str)] = &[
    ("pro", "deepseek-v4-pro"),
    ("flash", "deepseek-v4-flash"),
    ("chat", "deepseek-chat"),
    ("reasoner", "deepseek-reasoner"),
];

#[must_use]
pub fn resolve_model_alias(model: &str) -> String {
    let trimmed = model.trim();
    let lower = trimmed.to_ascii_lowercase();
    MODEL_REGISTRY
        .iter()
        .find_map(|(alias, canonical)| (*alias == lower).then_some(*canonical))
        .unwrap_or(trimmed)
        .to_string()
}

#[must_use]
pub fn metadata_for_model(model: &str) -> Option<ProviderMetadata> {
    let canonical = resolve_model_alias(model);
    if canonical.starts_with("deepseek") {
        return Some(ProviderMetadata {
            provider: ProviderKind::DeepSeek,
            auth_env: "DEEPSEEK_API_KEY",
            base_url_env: "DEEPSEEK_BASE_URL",
            default_base_url: openai_compat::DEFAULT_DEEPSEEK_BASE_URL,
        });
    }
    None
}

#[must_use]
pub fn provider_diagnostics_for_model(model: &str) -> ProviderDiagnostics {
    let resolved_model = resolve_model_alias(model);
    let metadata = metadata_for_model(&resolved_model).unwrap_or(ProviderMetadata {
        provider: ProviderKind::DeepSeek,
        auth_env: "DEEPSEEK_API_KEY",
        base_url_env: "DEEPSEEK_BASE_URL",
        default_base_url: openai_compat::DEFAULT_DEEPSEEK_BASE_URL,
    });
    let openai_compatible = true;
    let reasoning_model = openai_compat::is_reasoning_model(&resolved_model);

    ProviderDiagnostics {
        requested_model: model.to_string(),
        resolved_model: resolved_model.clone(),
        provider: metadata.provider,
        auth_env: metadata.auth_env,
        base_url_env: metadata.base_url_env,
        default_base_url: metadata.default_base_url,
        openai_compatible,
        reasoning_model,
        preserves_reasoning_content_in_history:
            openai_compat::model_requires_reasoning_content_in_history(&resolved_model),
        strips_tuning_params: reasoning_model,
        supports_stream_usage: true,
        honors_proxy_env: true,
        supports_extra_body_params: openai_compatible,
        preserves_slash_model_ids_on_custom_base_url: openai_compatible,
    }
}

#[must_use]
pub fn detect_provider_kind(_model: &str) -> ProviderKind {
    ProviderKind::DeepSeek
}

#[must_use]
pub const fn model_family_identity_for_kind(_kind: ProviderKind) -> runtime::ModelFamilyIdentity {
    runtime::ModelFamilyIdentity::Generic
}

#[must_use]
pub fn model_family_identity_for(model: &str) -> runtime::ModelFamilyIdentity {
    model_family_identity_for_kind(detect_provider_kind(model))
}

#[must_use]
pub fn provider_capabilities_for_model(model: &str) -> ProviderCapabilityReport {
    let metadata = metadata_for_model(model).unwrap_or(ProviderMetadata {
        provider: ProviderKind::DeepSeek,
        auth_env: "DEEPSEEK_API_KEY",
        base_url_env: "DEEPSEEK_BASE_URL",
        default_base_url: openai_compat::DEFAULT_DEEPSEEK_BASE_URL,
    });

    ProviderCapabilityReport {
        provider: metadata.provider,
        wire_protocol: ProviderWireProtocol::OpenAiChatCompletions,
        auth_env: metadata.auth_env,
        base_url_env: metadata.base_url_env,
        default_base_url: metadata.default_base_url,
        tool_calls: ProviderFeatureSupport::Supported,
        streaming: ProviderFeatureSupport::Supported,
        streaming_usage: ProviderFeatureSupport::Supported,
        prompt_cache: ProviderFeatureSupport::Unsupported,
        custom_parameters: ProviderFeatureSupport::Supported,
        reasoning_effort: ProviderFeatureSupport::Supported,
        reasoning_content_history: if openai_compat::model_requires_reasoning_content_in_history(
            model,
        ) {
            ProviderFeatureSupport::Supported
        } else {
            ProviderFeatureSupport::Unsupported
        },
        fixed_sampling_reasoning_models: ProviderFeatureSupport::Supported,
        web_search: ProviderFeatureSupport::PassthroughAsTool,
        web_fetch: ProviderFeatureSupport::PassthroughAsTool,
    }
}

#[must_use]
pub fn provider_diagnostics_for_request(request: &MessageRequest) -> Vec<ProviderDiagnostic> {
    let capabilities = provider_capabilities_for_model(&request.model);
    let mut diagnostics = Vec::new();

    if declares_tool(request, "web_search") {
        diagnostics.push(web_passthrough_diagnostic(
            "web_search_passthrough_tool",
            "web_search",
            capabilities.provider,
        ));
    }
    if declares_tool(request, "web_fetch") {
        diagnostics.push(web_passthrough_diagnostic(
            "web_fetch_passthrough_tool",
            "web_fetch",
            capabilities.provider,
        ));
    }

    diagnostics
}

#[must_use]
fn provider_label(_provider: ProviderKind) -> &'static str {
    "DeepSeek"
}

#[must_use]
fn declares_tool(request: &MessageRequest, tool_name: &str) -> bool {
    request.tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool.name.eq_ignore_ascii_case(tool_name))
    })
}

#[must_use]
fn web_passthrough_diagnostic(
    code: &'static str,
    tool_name: &'static str,
    _provider: ProviderKind,
) -> ProviderDiagnostic {
    ProviderDiagnostic {
        code,
        severity: ProviderDiagnosticSeverity::Info,
        message: format!(
            "`{tool_name}` is exposed to DeepSeek as a normal function tool, not as a provider-native web capability."
        ),
        action: format!(
            "Provide a local `{tool_name}` tool implementation or route through a provider adapter that explicitly supports native web tools."
        ),
    }
}

#[must_use]
pub fn max_tokens_for_model(model: &str) -> u32 {
    let canonical = resolve_model_alias(model);
    let heuristic = if canonical.contains("pro") {
        32_000
    } else {
        64_000
    };

    model_token_limit(model).map_or(heuristic, |limit| heuristic.min(limit.max_output_tokens))
}

/// Returns the effective max output tokens for a model, preferring a plugin
/// override when present. Falls back to [`max_tokens_for_model`] when the
/// override is `None`.
#[must_use]
pub fn max_tokens_for_model_with_override(model: &str, plugin_override: Option<u32>) -> u32 {
    plugin_override.unwrap_or_else(|| max_tokens_for_model(model))
}

#[must_use]
pub fn model_token_limit(model: &str) -> Option<ModelTokenLimit> {
    let canonical = resolve_model_alias(model);
    let base_model = canonical.rsplit('/').next().unwrap_or(canonical.as_str());
    match base_model {
        // DeepSeek V4 系列 — 1M 上下文窗口
        // Source: https://api-docs.deepseek.com/
        "deepseek-v4-pro" | "deepseek-v4-flash" => Some(ModelTokenLimit {
            max_output_tokens: 32_000,
            context_window_tokens: 1_000_000,
        }),
        // DeepSeek V3 (deepseek-chat) — 64K 上下文窗口
        "deepseek-chat" => Some(ModelTokenLimit {
            max_output_tokens: 8_192,
            context_window_tokens: 64_000,
        }),
        // DeepSeek R1 (deepseek-reasoner) — 128K 上下文窗口
        "deepseek-reasoner" => Some(ModelTokenLimit {
            max_output_tokens: 8_192,
            context_window_tokens: 128_000,
        }),
        _ => None,
    }
}

pub fn preflight_message_request(request: &MessageRequest) -> Result<(), ApiError> {
    let Some(limit) = model_token_limit(&request.model) else {
        return Ok(());
    };

    let estimated_input_tokens = estimate_message_request_input_tokens(request);
    let estimated_total_tokens = estimated_input_tokens.saturating_add(request.max_tokens);
    if estimated_total_tokens > limit.context_window_tokens {
        return Err(ApiError::ContextWindowExceeded {
            model: resolve_model_alias(&request.model),
            estimated_input_tokens,
            requested_output_tokens: request.max_tokens,
            estimated_total_tokens,
            context_window_tokens: limit.context_window_tokens,
        });
    }

    Ok(())
}

fn estimate_message_request_input_tokens(request: &MessageRequest) -> u32 {
    let mut estimate = estimate_serialized_tokens(&request.messages);
    estimate = estimate.saturating_add(estimate_serialized_tokens(&request.system));
    estimate = estimate.saturating_add(estimate_serialized_tokens(&request.tools));
    estimate = estimate.saturating_add(estimate_serialized_tokens(&request.tool_choice));
    estimate
}

fn estimate_serialized_tokens<T: Serialize>(value: &T) -> u32 {
    serde_json::to_vec(value)
        .ok()
        .map_or(0, |bytes| (bytes.len() / 4 + 1) as u32)
}

/// Parse a `.env` file body into key/value pairs using a minimal `KEY=VALUE`
/// grammar. Lines that are blank, start with `#`, or do not contain `=` are
/// ignored. Surrounding double or single quotes are stripped from the value.
/// An optional leading `export ` prefix on the key is also stripped so files
/// shared with shell `source` workflows still parse cleanly.
pub(crate) fn parse_dotenv(content: &str) -> std::collections::HashMap<String, String> {
    let mut values = std::collections::HashMap::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let trimmed_key = raw_key.trim();
        let key = trimmed_key
            .strip_prefix("export ")
            .map_or(trimmed_key, str::trim)
            .to_string();
        if key.is_empty() {
            continue;
        }
        let trimmed_value = raw_value.trim();
        let unquoted = if (trimmed_value.starts_with('"') && trimmed_value.ends_with('"')
            || trimmed_value.starts_with('\'') && trimmed_value.ends_with('\''))
            && trimmed_value.len() >= 2
        {
            &trimmed_value[1..trimmed_value.len() - 1]
        } else {
            trimmed_value
        };
        values.insert(key, unquoted.to_string());
    }
    values
}

/// Load and parse a `.env` file from the given path. Missing files yield
/// `None` instead of an error so callers can use this as a soft fallback.
pub(crate) fn load_dotenv_file(
    path: &std::path::Path,
) -> Option<std::collections::HashMap<String, String>> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(parse_dotenv(&content))
}

/// Look up `key` in a `.env` file located in the current working directory.
/// Returns `None` when the file is missing, the key is absent, or the value
/// is empty.
pub(crate) fn dotenv_value(key: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let values = load_dotenv_file(&cwd.join(".env"))?;
    values.get(key).filter(|value| !value.is_empty()).cloned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::types::{
        InputContentBlock, InputMessage, MessageRequest, SystemContent, ToolChoice, ToolDefinition,
    };

    use super::{
        detect_provider_kind, max_tokens_for_model, max_tokens_for_model_with_override,
        model_family_identity_for, model_family_identity_for_kind, model_token_limit, parse_dotenv,
        preflight_message_request, provider_capabilities_for_model,
        provider_diagnostics_for_request, resolve_model_alias, ProviderFeatureSupport,
        ProviderKind, ProviderWireProtocol,
    };

    #[test]
    fn returns_context_window_metadata_for_deepseek_models() {
        // deepseek-v4-pro — 1M context
        let v4pro = model_token_limit("deepseek-v4-pro")
            .expect("deepseek-v4-pro should have token limit metadata");
        assert_eq!(v4pro.max_output_tokens, 32_000);
        assert_eq!(v4pro.context_window_tokens, 1_000_000);

        // deepseek-v4-flash — 1M context
        let v4flash = model_token_limit("deepseek-v4-flash")
            .expect("deepseek-v4-flash should have token limit metadata");
        assert_eq!(v4flash.max_output_tokens, 32_000);
        assert_eq!(v4flash.context_window_tokens, 1_000_000);

        // deepseek-chat (V3) — 64K context
        let v3 = model_token_limit("deepseek-chat")
            .expect("deepseek-chat should have token limit metadata");
        assert_eq!(v3.max_output_tokens, 8_192);
        assert_eq!(v3.context_window_tokens, 64_000);

        // deepseek-reasoner (R1) — 128K context
        let r1 = model_token_limit("deepseek-reasoner")
            .expect("deepseek-reasoner should have token limit metadata");
        assert_eq!(r1.max_output_tokens, 8_192);
        assert_eq!(r1.context_window_tokens, 128_000);
    }

    #[test]
    fn preflight_blocks_oversized_requests_for_deepseek_v4_models() {
        // 5M chars → ~1.25M estimated tokens → exceeds 1M context window
        let request = MessageRequest {
            model: "deepseek-v4-pro".to_string(),
            max_tokens: 32_000,
            messages: vec![InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::Text {
                    text: "x".repeat(5_000_000), // ~1.25M estimated tokens
                }],
            }],
            system: None,
            tools: None,
            tool_choice: None,
            stream: true,
            ..Default::default()
        };

        let error = preflight_message_request(&request)
            .expect_err("oversized request should be rejected for deepseek-v4 models");

        match error {
            crate::error::ApiError::ContextWindowExceeded {
                model,
                context_window_tokens,
                ..
            } => {
                assert_eq!(model, "deepseek-v4-pro");
                assert_eq!(context_window_tokens, 1_000_000);
            }
            other => panic!("expected context-window preflight failure, got {other:?}"),
        }
    }

    #[test]
    fn deepseek_v4_with_prefix_resolves_context_window() {
        // deepseek/deepseek-v4-pro 前缀应能解析
        let limit = model_token_limit("deepseek/deepseek-v4-pro")
            .expect("deepseek/deepseek-v4-pro should resolve to deepseek-v4-pro limits");
        assert_eq!(
            limit.context_window_tokens, 1_000_000,
            "deepseek/deepseek-v4-pro should have 1M context window"
        );
    }

    #[test]
    fn resolves_deepseek_aliases() {
        assert_eq!(resolve_model_alias("pro"), "deepseek-v4-pro");
        assert_eq!(resolve_model_alias("flash"), "deepseek-v4-flash");
        assert_eq!(resolve_model_alias("chat"), "deepseek-chat");
        assert_eq!(resolve_model_alias("reasoner"), "deepseek-reasoner");
        // Case insensitive
        assert_eq!(resolve_model_alias("PRO"), "deepseek-v4-pro");
    }

    #[test]
    fn detects_provider_kind_always_returns_deepseek() {
        assert_eq!(
            detect_provider_kind("deepseek-v4-pro"),
            ProviderKind::DeepSeek
        );
        assert_eq!(detect_provider_kind("anything"), ProviderKind::DeepSeek);
    }

    #[test]
    fn maps_provider_kind_to_model_family_identity() {
        let deepseek = ProviderKind::DeepSeek;
        let identity = model_family_identity_for_kind(deepseek);
        assert_eq!(identity, runtime::ModelFamilyIdentity::Generic);
    }

    #[test]
    fn maps_model_name_to_model_family_identity() {
        let deepseek_model = "deepseek-v4-pro";
        let identity = model_family_identity_for(deepseek_model);
        assert_eq!(identity, runtime::ModelFamilyIdentity::Generic);
    }

    #[test]
    fn provider_capability_matrix_for_deepseek() {
        let deepseek = provider_capabilities_for_model("deepseek-v4-pro");
        assert_eq!(deepseek.provider, ProviderKind::DeepSeek);
        assert_eq!(
            deepseek.wire_protocol,
            ProviderWireProtocol::OpenAiChatCompletions
        );
        assert_eq!(deepseek.auth_env, "DEEPSEEK_API_KEY");
        assert_eq!(deepseek.streaming_usage, ProviderFeatureSupport::Supported);
        assert_eq!(deepseek.reasoning_effort, ProviderFeatureSupport::Supported);
        assert_eq!(
            deepseek.web_search,
            ProviderFeatureSupport::PassthroughAsTool
        );
        assert_eq!(
            deepseek.web_fetch,
            ProviderFeatureSupport::PassthroughAsTool
        );
    }

    #[test]
    fn provider_capability_reasoning_content_history_for_deepseek_v4() {
        let deepseek = provider_capabilities_for_model("deepseek-v4-pro");
        assert_eq!(
            deepseek.reasoning_content_history,
            ProviderFeatureSupport::Unsupported
        );
    }

    #[test]
    fn provider_diagnostics_explain_deepseek_reasoning_and_web_tool_passthrough() {
        let request = MessageRequest {
            model: "deepseek-v4-pro".to_string(),
            max_tokens: 1024,
            messages: vec![InputMessage::user_text("research this")],
            tools: Some(vec![
                ToolDefinition {
                    name: "web_search".to_string(),
                    description: Some("Search the web".to_string()),
                    input_schema: json!({"type": "object"}),
                    cache_control: None,
                },
                ToolDefinition {
                    name: "web_fetch".to_string(),
                    description: Some("Fetch a URL".to_string()),
                    input_schema: json!({"type": "object"}),
                    cache_control: None,
                },
            ]),
            stream: true,
            ..Default::default()
        };

        let diagnostics = provider_diagnostics_for_request(&request);
        let codes = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect::<Vec<_>>();

        assert!(codes.contains(&"web_search_passthrough_tool"));
        assert!(codes.contains(&"web_fetch_passthrough_tool"));
    }

    #[test]
    fn provider_diagnostics_for_model_deepseek() {
        let diagnostics = super::provider_diagnostics_for_model("deepseek-v4-pro");

        assert_eq!(diagnostics.provider, ProviderKind::DeepSeek);
        assert_eq!(diagnostics.auth_env, "DEEPSEEK_API_KEY");
        assert!(diagnostics.openai_compatible);
        assert!(!diagnostics.preserves_reasoning_content_in_history);
        assert!(diagnostics.supports_extra_body_params);
        assert!(diagnostics.honors_proxy_env);
        assert!(diagnostics.preserves_slash_model_ids_on_custom_base_url);
    }

    #[test]
    fn keeps_existing_max_token_heuristic() {
        assert_eq!(max_tokens_for_model("deepseek-v4-pro"), 32_000);
    }

    #[test]
    fn plugin_config_max_output_tokens_overrides_model_default() {
        // given
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("api-plugin-max-tokens-{nanos}"));
        let cwd = root.join("project");
        let home = root.join("home").join(".claw");
        std::fs::create_dir_all(cwd.join(".claw")).expect("project config dir");
        std::fs::create_dir_all(&home).expect("home config dir");
        std::fs::write(
            home.join("settings.json"),
            r#"{
              "plugins": {
                "maxOutputTokens": 12345
              }
            }"#,
        )
        .expect("write plugin settings");

        // when
        let loaded = runtime::ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");
        let plugin_override = loaded.plugins().max_output_tokens();
        let effective = max_tokens_for_model_with_override("deepseek-v4-pro", plugin_override);

        // then
        assert_eq!(plugin_override, Some(12345));
        assert_eq!(effective, 12345);

        std::fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn max_tokens_for_model_with_override_falls_back_when_plugin_unset() {
        // given
        let plugin_override: Option<u32> = None;

        // when
        let effective = max_tokens_for_model_with_override("deepseek-v4-pro", plugin_override);

        // then
        assert_eq!(effective, max_tokens_for_model("deepseek-v4-pro"));
        assert_eq!(effective, 32_000);
    }

    #[test]
    fn preflight_blocks_requests_that_exceed_the_model_context_window() {
        let request = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 32_000,
            messages: vec![InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::Text {
                    // 5M chars → ~1.25M estimated tokens → exceeds the 1M context window
                    text: "x".repeat(5_000_000),
                }],
            }],
            system: Some(SystemContent::from_text("Keep the answer short.")),
            tools: Some(vec![ToolDefinition {
                name: "weather".to_string(),
                description: Some("Fetches weather".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                }),
                cache_control: None,
            }]),
            tool_choice: Some(ToolChoice::Auto),
            stream: true,
            ..Default::default()
        };

        let error = preflight_message_request(&request)
            .expect_err("oversized request should be rejected before the provider call");

        match error {
            crate::error::ApiError::ContextWindowExceeded {
                model,
                estimated_input_tokens,
                requested_output_tokens,
                estimated_total_tokens,
                context_window_tokens,
            } => {
                assert_eq!(model, "deepseek-v4-flash");
                assert!(estimated_input_tokens > 136_000);
                assert_eq!(requested_output_tokens, 32_000);
                assert!(estimated_total_tokens > context_window_tokens);
                assert_eq!(context_window_tokens, 1_000_000);
            }
            other => panic!("expected context-window preflight failure, got {other:?}"),
        }
    }

    #[test]
    fn preflight_skips_unknown_models() {
        let request = MessageRequest {
            model: "unknown-model".to_string(),
            max_tokens: 64_000,
            messages: vec![InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::Text {
                    text: "x".repeat(600_000),
                }],
            }],
            system: None,
            tools: None,
            tool_choice: None,
            stream: false,
            ..Default::default()
        };

        preflight_message_request(&request)
            .expect("models without context metadata should skip the guarded preflight");
    }

    #[test]
    fn parse_dotenv_extracts_keys_handles_comments_quotes_and_export_prefix() {
        // given
        let body = "\
# this is a comment

DEEPSEEK_API_KEY=plain-value
OPENAI_API_KEY=\"quoted-value\"
export GROK_API_KEY=exported-value
   PADDED_KEY  =  padded-value  
EMPTY_VALUE=
NO_EQUALS_LINE
";

        // when
        let values = parse_dotenv(body);

        // then
        assert_eq!(
            values.get("DEEPSEEK_API_KEY").map(String::as_str),
            Some("plain-value")
        );
        assert_eq!(
            values.get("OPENAI_API_KEY").map(String::as_str),
            Some("quoted-value")
        );
        assert_eq!(
            values.get("GROK_API_KEY").map(String::as_str),
            Some("exported-value")
        );
        assert_eq!(
            values.get("PADDED_KEY").map(String::as_str),
            Some("padded-value")
        );
        assert_eq!(values.get("EMPTY_VALUE").map(String::as_str), Some(""));
        assert!(!values.contains_key("NO_EQUALS_LINE"));
        assert!(!values.contains_key("# this is a comment"));
    }

    #[test]
    fn load_dotenv_file_reads_keys_from_disk_and_returns_none_when_missing() {
        // given
        let temp_root = std::env::temp_dir().join(format!(
            "api-dotenv-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        std::fs::create_dir_all(&temp_root).expect("create temp dir");
        let env_path = temp_root.join(".env");
        std::fs::write(
            &env_path,
            "DEEPSEEK_API_KEY=secret-from-file\n# comment\nOPENAI_API_KEY=\"openai-secret\"\n",
        )
        .expect("write .env");
        let missing_path = temp_root.join("does-not-exist.env");

        // when
        let loaded = super::load_dotenv_file(&env_path).expect("file should load");
        let missing = super::load_dotenv_file(&missing_path);

        // then
        assert_eq!(
            loaded.get("DEEPSEEK_API_KEY").map(String::as_str),
            Some("secret-from-file")
        );
        assert_eq!(
            loaded.get("OPENAI_API_KEY").map(String::as_str),
            Some("openai-secret")
        );
        assert!(missing.is_none());

        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
