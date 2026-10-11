use crate::error::ApiError;
use crate::providers::openai_compat::{self, OpenAiCompatClient, OpenAiCompatConfig};
use crate::providers::{self, ProviderKind};
use crate::types::{MessageRequest, MessageResponse, StreamEvent};

/// Single-provider client wrapping the DeepSeek OpenAI-compatible backend.
#[derive(Debug, Clone)]
pub struct ProviderClient {
    inner: OpenAiCompatClient,
}

impl ProviderClient {
    pub fn from_model(model: &str) -> Result<Self, ApiError> {
        let _resolved_model = providers::resolve_model_alias(model);
        Ok(Self {
            inner: OpenAiCompatClient::from_env(OpenAiCompatConfig::deepseek())?,
        })
    }

    /// Compatibility shim — accepts an optional auth token for callers that
    /// previously supplied an `AuthSource`. DeepSeek only uses
    /// `DEEPSEEK_API_KEY` via `from_env`, so the argument is ignored.
    pub fn from_model_with_auth(model: &str, _auth: Option<String>) -> Result<Self, ApiError> {
        Self::from_model(model)
    }

    #[must_use]
    pub const fn provider_kind(&self) -> ProviderKind {
        ProviderKind::DeepSeek
    }

    pub async fn send_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        self.inner.send_message(request).await
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageStream, ApiError> {
        self.inner
            .stream_message(request)
            .await
            .map(MessageStream::OpenAiCompat)
    }
}

#[derive(Debug)]
pub enum MessageStream {
    OpenAiCompat(openai_compat::MessageStream),
}

impl MessageStream {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::OpenAiCompat(stream) => stream.request_id(),
        }
    }

    /// 首 chunk 延迟(毫秒) —— 对齐 OTel
    /// `gen_ai.client.inference.time_to_first_chunk`。
    ///
    /// `ProviderClient::stream_message` 返回后、首个 `next_event` 返回前为 `None`;
    /// 首 chunk 到达后恒为 `Some`(幂等,只记首次)。
    #[must_use]
    pub fn time_to_first_chunk_ms(&self) -> Option<u64> {
        match self {
            Self::OpenAiCompat(stream) => stream.time_to_first_chunk_ms(),
        }
    }

    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ApiError> {
        match self {
            Self::OpenAiCompat(stream) => stream.next_event().await,
        }
    }
}

#[must_use]
pub fn read_base_url() -> String {
    openai_compat::read_base_url(OpenAiCompatConfig::deepseek())
}

#[cfg(test)]
mod tests {
    use crate::providers::{detect_provider_kind, resolve_model_alias, ProviderKind};

    #[test]
    fn resolves_deepseek_aliases() {
        assert_eq!(resolve_model_alias("pro"), "deepseek-v4-pro");
        assert_eq!(resolve_model_alias("flash"), "deepseek-v4-flash");
    }

    #[test]
    fn provider_detection_always_returns_deepseek() {
        assert_eq!(
            detect_provider_kind("deepseek-v4-pro"),
            ProviderKind::DeepSeek
        );
        assert_eq!(detect_provider_kind("anything"), ProviderKind::DeepSeek);
    }
}
