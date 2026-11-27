use crate::error::ApiError;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::providers::openai_compat::{self, OpenAiCompatClient, OpenAiCompatConfig};
use crate::providers::{self, trae, ProviderKind};
use crate::types::{MessageRequest, MessageResponse, StreamEvent};

/// 凭证过期前的提前重建窗口：避免请求发出瞬间正好过期。
const EXPIRY_SKEW_MS: u64 = 5 * 60 * 1000;
/// 无法解析 JWT `exp`（不透明令牌）时的保守缓存时长。
const UNKNOWN_TTL_MS: u64 = 5 * 60 * 1000;

#[derive(Clone)]
struct CachedProvider {
    client: ProviderClient,
    expires_at_ms: Option<u64>,
    built_at_ms: u64,
}

fn provider_cache() -> &'static Mutex<HashMap<String, CachedProvider>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedProvider>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// 缓存条目是否仍可复用：JWT 未进入提前重建窗口；无 `exp` 时按短 TTL。
fn cache_entry_is_fresh(expires_at_ms: Option<u64>, built_at_ms: u64, now: u64) -> bool {
    match expires_at_ms {
        Some(expires) => now.saturating_add(EXPIRY_SKEW_MS) < expires,
        None => now < built_at_ms.saturating_add(UNKNOWN_TTL_MS),
    }
}

fn cached_trae(model: &str) -> Option<ProviderClient> {
    let cache = provider_cache().lock().ok()?;
    let entry = cache.get(model)?;
    cache_entry_is_fresh(entry.expires_at_ms, entry.built_at_ms, now_ms())
        .then(|| entry.client.clone())
}

fn store_trae(model: &str, client: &ProviderClient) {
    let expires_at_ms = match &client.inner {
        ProviderInner::Trae(trae_client) => trae_client.expires_at_ms(),
        ProviderInner::OpenAiCompat(_) => None,
    };
    if let Ok(mut cache) = provider_cache().lock() {
        cache.insert(
            model.to_string(),
            CachedProvider {
                client: client.clone(),
                expires_at_ms,
                built_at_ms: now_ms(),
            },
        );
    }
}

fn invalidate(name: &str) {
    if let Ok(mut cache) = provider_cache().lock() {
        cache.remove(name);
    }
}

/// Provider-dispatching client. `deepseek/...` models keep using the
/// OpenAI-compatible DeepSeek endpoint; `trae/...` models use the native
/// Trae chat endpoint (Trae points/credits).
#[derive(Debug, Clone)]
pub struct ProviderClient {
    inner: ProviderInner,
}

#[derive(Debug, Clone)]
enum ProviderInner {
    OpenAiCompat(OpenAiCompatClient),
    Trae(trae::TraeClient),
}

impl ProviderClient {
    pub fn from_model(model: &str) -> Result<Self, ApiError> {
        let trimmed = model.trim();
        if trimmed.starts_with("trae/") {
            // Trae 每次构造都要读/解密 storage.json 并新建 HTTP 连接池，
            // 这里按 JWT 过期时间缓存：正常轮次直接复用，临近过期才重建。
            if let Some(cached) = cached_trae(trimmed) {
                return Ok(cached);
            }
            let client = Self::build_uncached(trimmed)?;
            store_trae(trimmed, &client);
            return Ok(client);
        }
        Self::build_uncached(model)
    }

    fn build_uncached(model: &str) -> Result<Self, ApiError> {
        let trimmed = model.trim();
        if trimmed.starts_with("trae/") {
            return Ok(Self {
                inner: ProviderInner::Trae(trae::TraeClient::from_env()?),
            });
        }
        let _resolved_model = providers::resolve_model_alias(model);
        Ok(Self {
            inner: ProviderInner::OpenAiCompat(OpenAiCompatClient::from_env(
                OpenAiCompatConfig::deepseek(),
            )?),
        })
    }

    fn is_trae(&self) -> bool {
        matches!(&self.inner, ProviderInner::Trae(_))
    }

    /// 丢弃缓存并重读本地凭证。凭证仍不可用时返回 `None`（保留原始 401）。
    fn refresh_trae(model: &str) -> Option<Self> {
        let key = model.trim();
        invalidate(key);
        Self::from_model(key).ok()
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
        let result = match &self.inner {
            ProviderInner::OpenAiCompat(client) => client.send_message(request).await,
            ProviderInner::Trae(client) => client.send_message(request).await,
        };
        let error = match result {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        // 凭证可能已被 Trae 桌面端轮换/吊销：丢弃缓存，重读 storage.json 后重试一次。
        if self.is_trae() && error.is_auth_error() {
            if let Some(fresh) = Self::refresh_trae(&request.model) {
                if let ProviderInner::Trae(client) = &fresh.inner {
                    return client.send_message(request).await;
                }
            }
        }
        Err(error)
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageStream, ApiError> {
        let result = match &self.inner {
            ProviderInner::OpenAiCompat(client) => client
                .stream_message(request)
                .await
                .map(MessageStream::OpenAiCompat),
            ProviderInner::Trae(client) => client
                .stream_message(request)
                .await
                .map(MessageStream::Trae),
        };
        let error = match result {
            Ok(stream) => return Ok(stream),
            Err(error) => error,
        };
        if self.is_trae() && error.is_auth_error() {
            if let Some(fresh) = Self::refresh_trae(&request.model) {
                if let ProviderInner::Trae(client) = &fresh.inner {
                    return client
                        .stream_message(request)
                        .await
                        .map(MessageStream::Trae);
                }
            }
        }
        Err(error)
    }
}

#[derive(Debug)]
pub enum MessageStream {
    OpenAiCompat(openai_compat::MessageStream),
    Trae(trae::MessageStream),
}

impl MessageStream {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::OpenAiCompat(stream) => stream.request_id(),
            Self::Trae(stream) => stream.request_id(),
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
            Self::Trae(stream) => stream.time_to_first_chunk_ms(),
        }
    }

    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ApiError> {
        match self {
            Self::OpenAiCompat(stream) => stream.next_event().await,
            Self::Trae(stream) => stream.next_event().await,
        }
    }
}

#[must_use]
pub fn read_base_url() -> String {
    openai_compat::read_base_url(OpenAiCompatConfig::deepseek())
}

#[cfg(test)]
mod tests {
    use super::{cache_entry_is_fresh, EXPIRY_SKEW_MS, UNKNOWN_TTL_MS};
    use crate::providers::{detect_provider_kind, resolve_model_alias, ProviderKind};

    #[test]
    fn resolves_deepseek_aliases() {
        assert_eq!(resolve_model_alias("pro"), "deepseek-flash");
        assert_eq!(resolve_model_alias("flash"), "deepseek-flash");
    }

    #[test]
    fn provider_cache_entry_respects_jwt_expiry_and_skew() {
        let now = 1_700_000_000_000;
        // 距过期仍大于 5 分钟：可复用。
        assert!(cache_entry_is_fresh(
            Some(now + EXPIRY_SKEW_MS + 1),
            now,
            now
        ));
        // 进入提前重建窗口：必须重建。
        assert!(!cache_entry_is_fresh(Some(now + EXPIRY_SKEW_MS), now, now));
        assert!(!cache_entry_is_fresh(Some(now - 1), now, now));
        // 无 exp：按 5 分钟保守 TTL。
        assert!(cache_entry_is_fresh(None, now, now));
        assert!(!cache_entry_is_fresh(None, now - UNKNOWN_TTL_MS, now));
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
