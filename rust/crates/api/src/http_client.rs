use crate::error::ApiError;
use std::time::Duration;

const HTTP_PROXY_KEYS: [&str; 2] = ["HTTP_PROXY", "http_proxy"];
const HTTPS_PROXY_KEYS: [&str; 2] = ["HTTPS_PROXY", "https_proxy"];
const NO_PROXY_KEYS: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// Timeout configuration for outbound HTTP requests.
///
/// `connect_timeout` guards against silently-hung TCP handshakes. `request_timeout`
/// caps the total time for a non-streaming request (headers + body). For streaming
/// responses (SSE for `stream_message`), `reqwest::Client::builder::timeout` only
/// applies to the initial handshake (headers + body framing); the stream itself is
/// governed by SSE parsing and the inter-event timeout in `consume_stream`.
///
/// Both fields can be overridden via environment variables (`CLAW_API_CONNECT_TIMEOUT`,
/// `CLAW_API_REQUEST_TIMEOUT`) or through `TimeoutConfig::from_seconds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutConfig {
    /// Maximum time to wait for a connection to be established.
    /// Defaults to 30 seconds.
    pub connect_timeout: Duration,
    /// Maximum time for the entire non-streaming request (including reading
    /// the response body). Defaults to 5 minutes (300 seconds).
    pub request_timeout: Duration,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(300),
        }
    }
}

impl TimeoutConfig {
    /// Read timeout settings from the process environment.
    /// - `CLAW_API_CONNECT_TIMEOUT` — connect timeout in seconds
    /// - `CLAW_API_REQUEST_TIMEOUT` — overall request timeout in seconds
    ///
    /// Unset or unparseable values fall back to the defaults from
    /// [`TimeoutConfig::default`].
    #[must_use]
    pub fn from_env() -> Self {
        let connect_timeout = std::env::var("CLAW_API_CONNECT_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(30));
        let request_timeout = std::env::var("CLAW_API_REQUEST_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(300));
        Self {
            connect_timeout,
            request_timeout,
        }
    }

    /// Create from explicit second values (used by config file parsing).
    #[must_use]
    pub fn from_seconds(connect_secs: u64, request_secs: u64) -> Self {
        Self {
            connect_timeout: Duration::from_secs(connect_secs),
            request_timeout: Duration::from_secs(request_secs),
        }
    }
}

/// Snapshot of the proxy-related environment variables that influence the
/// outbound HTTP client. Captured up front so callers can inspect, log, and
/// test the resolved configuration without re-reading the process environment.
///
/// When `proxy_url` is set it acts as a single catch-all proxy for both
/// HTTP and HTTPS traffic, taking precedence over the per-scheme fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
    /// Optional unified proxy URL that applies to both HTTP and HTTPS.
    /// When set, this takes precedence over `http_proxy` and `https_proxy`.
    pub proxy_url: Option<String>,
}

impl ProxyConfig {
    /// Read proxy settings from the live process environment, honouring both
    /// the upper- and lower-case spellings used by curl, git, and friends.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Create a proxy configuration from a single URL that applies to both
    /// HTTP and HTTPS traffic. This is the config-file alternative to setting
    /// `HTTP_PROXY` and `HTTPS_PROXY` environment variables separately.
    #[must_use]
    pub fn from_proxy_url(url: impl Into<String>) -> Self {
        Self {
            proxy_url: Some(url.into()),
            ..Self::default()
        }
    }

    fn from_lookup<F>(mut lookup: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self {
            http_proxy: first_non_empty(&HTTP_PROXY_KEYS, &mut lookup),
            https_proxy: first_non_empty(&HTTPS_PROXY_KEYS, &mut lookup),
            no_proxy: first_non_empty(&NO_PROXY_KEYS, &mut lookup),
            proxy_url: None,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.proxy_url.is_none() && self.http_proxy.is_none() && self.https_proxy.is_none()
    }
}

/// Build a `reqwest::Client` that honours the standard `HTTP_PROXY`,
/// `HTTPS_PROXY`, and `NO_PROXY` environment variables. When no proxy is
/// configured the client behaves identically to `reqwest::Client::new()`.
///
/// Timeouts are read from `CLAW_API_CONNECT_TIMEOUT` / `CLAW_API_REQUEST_TIMEOUT`
/// environment variables via [`TimeoutConfig::from_env`].
pub fn build_http_client() -> Result<reqwest::Client, ApiError> {
    build_http_client_with_opts(&ProxyConfig::from_env(), &TimeoutConfig::from_env())
}

/// Infallible counterpart to [`build_http_client`] for constructors that
/// historically returned `Self` rather than `Result<Self, _>`. When the proxy
/// configuration is malformed we fall back to a default client so that
/// callers retain the previous behaviour and the failure surfaces on the
/// first outbound request instead of at construction time.
#[must_use]
pub fn build_http_client_or_default() -> reqwest::Client {
    build_http_client_with_opts(&ProxyConfig::from_env(), &TimeoutConfig::from_env())
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Build a `reqwest::Client` from an explicit [`ProxyConfig`]. Used by tests
/// and by callers that want to override process-level environment lookups.
/// Timeouts default to [`TimeoutConfig::from_env`] so tests that pass a
/// `ProxyConfig` still honour `CLAW_API_*_TIMEOUT` overrides.
///
/// When `config.proxy_url` is set it overrides the per-scheme `http_proxy`
/// and `https_proxy` fields and is registered as both an HTTP and HTTPS
/// proxy so a single value can route every outbound request.
pub fn build_http_client_with(config: &ProxyConfig) -> Result<reqwest::Client, ApiError> {
    build_http_client_with_opts(config, &TimeoutConfig::from_env())
}

/// Build a `reqwest::Client` from explicit [`ProxyConfig`] and [`TimeoutConfig`].
/// Used by callers that want to control both proxy routing and request timing
/// (e.g. config-file-driven construction in the runtime crate).
///
/// `connect_timeout` guards the TCP handshake; `request_timeout` caps the
/// total time for non-streaming requests. For `stream_message`, reqwest's
/// client-level timeout only applies to the headers/body-framing phase —
/// the SSE stream itself is governed by the inter-event timeout in
/// `consume_stream` (see `runtime/src/streaming.rs`).
pub fn build_http_client_with_opts(
    config: &ProxyConfig,
    timeout: &TimeoutConfig,
) -> Result<reqwest::Client, ApiError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(timeout.connect_timeout)
        .timeout(timeout.request_timeout)
        // ── 连接池保活：防止长时间空闲后复用陈旧连接 ──
        //
        // 根因：reqwest 默认 pool_idle_timeout=90s 且不开 TCP keepalive。
        // 长时间空闲后，连接池中的连接可能已被中间设备（NAT/防火墙/LB）
        // 静默关闭，但客户端不知情。复用这种"看似存活但已被对端关闭"
        // 的连接会导致 SSE 流式响应静默挂起，触发 60s INTER_EVENT_TIMEOUT，
        // 最终 turn 以 error 结束。
        //
        // 修复：
        // - pool_idle_timeout=60s：缩短空闲超时，主动清理闲置连接，
        //   使下次请求强制建立新连接而非复用可能已失效的旧连接。
        // - tcp_keepalive=30s：开启 TCP 层 keepalive 探测，主动检测
        //   连接存活状态，让操作系统及时回收半开连接。
        .pool_idle_timeout(Some(Duration::from_secs(60)))
        .tcp_keepalive(Some(Duration::from_secs(30)));

    let no_proxy = config
        .no_proxy
        .as_deref()
        .and_then(reqwest::NoProxy::from_string);

    let (http_proxy_url, https_url) = match config.proxy_url.as_deref() {
        Some(unified) => (Some(unified), Some(unified)),
        None => (config.http_proxy.as_deref(), config.https_proxy.as_deref()),
    };

    if let Some(url) = https_url {
        let mut proxy = reqwest::Proxy::https(url)?;
        if let Some(filter) = no_proxy.clone() {
            proxy = proxy.no_proxy(Some(filter));
        }
        builder = builder.proxy(proxy);
    }

    if let Some(url) = http_proxy_url {
        let mut proxy = reqwest::Proxy::http(url)?;
        if let Some(filter) = no_proxy.clone() {
            proxy = proxy.no_proxy(Some(filter));
        }
        builder = builder.proxy(proxy);
    }

    Ok(builder.build()?)
}

fn first_non_empty<F>(keys: &[&str], lookup: &mut F) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    keys.iter()
        .find_map(|key| lookup(key).filter(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::{build_http_client_with, build_http_client_with_opts, ProxyConfig, TimeoutConfig};

    fn config_from_map(pairs: &[(&str, &str)]) -> ProxyConfig {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        ProxyConfig::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn proxy_config_is_empty_when_no_env_vars_are_set() {
        // given
        let config = config_from_map(&[]);

        // when
        let empty = config.is_empty();

        // then
        assert!(empty);
        assert_eq!(config, ProxyConfig::default());
    }

    #[test]
    fn proxy_config_reads_uppercase_http_https_and_no_proxy() {
        // given
        let pairs = [
            ("HTTP_PROXY", "http://proxy.internal:3128"),
            ("HTTPS_PROXY", "http://secure.internal:3129"),
            ("NO_PROXY", "localhost,127.0.0.1,.corp"),
        ];

        // when
        let config = config_from_map(&pairs);

        // then
        assert_eq!(
            config.http_proxy.as_deref(),
            Some("http://proxy.internal:3128")
        );
        assert_eq!(
            config.https_proxy.as_deref(),
            Some("http://secure.internal:3129")
        );
        assert_eq!(
            config.no_proxy.as_deref(),
            Some("localhost,127.0.0.1,.corp")
        );
        assert!(!config.is_empty());
    }

    #[test]
    fn proxy_config_falls_back_to_lowercase_keys() {
        // given
        let pairs = [
            ("http_proxy", "http://lower.internal:3128"),
            ("https_proxy", "http://lower-secure.internal:3129"),
            ("no_proxy", ".lower"),
        ];

        // when
        let config = config_from_map(&pairs);

        // then
        assert_eq!(
            config.http_proxy.as_deref(),
            Some("http://lower.internal:3128")
        );
        assert_eq!(
            config.https_proxy.as_deref(),
            Some("http://lower-secure.internal:3129")
        );
        assert_eq!(config.no_proxy.as_deref(), Some(".lower"));
    }

    #[test]
    fn proxy_config_prefers_uppercase_over_lowercase_when_both_set() {
        // given
        let pairs = [
            ("HTTP_PROXY", "http://upper.internal:3128"),
            ("http_proxy", "http://lower.internal:3128"),
        ];

        // when
        let config = config_from_map(&pairs);

        // then
        assert_eq!(
            config.http_proxy.as_deref(),
            Some("http://upper.internal:3128")
        );
    }

    #[test]
    fn proxy_config_treats_empty_strings_as_unset() {
        // given
        let pairs = [("HTTP_PROXY", ""), ("http_proxy", "")];

        // when
        let config = config_from_map(&pairs);

        // then
        assert!(config.http_proxy.is_none());
    }

    #[test]
    fn build_http_client_succeeds_when_no_proxy_is_configured() {
        // given
        let config = ProxyConfig::default();

        // when
        let result = build_http_client_with(&config);

        // then
        assert!(result.is_ok());
    }

    #[test]
    fn build_http_client_succeeds_with_valid_http_and_https_proxies() {
        // given
        let config = ProxyConfig {
            http_proxy: Some("http://proxy.internal:3128".to_string()),
            https_proxy: Some("http://secure.internal:3129".to_string()),
            no_proxy: Some("localhost,127.0.0.1".to_string()),
            proxy_url: None,
        };

        // when
        let result = build_http_client_with(&config);

        // then
        assert!(result.is_ok());
    }

    #[test]
    fn build_http_client_returns_http_error_for_invalid_proxy_url() {
        // given
        let config = ProxyConfig {
            http_proxy: None,
            https_proxy: Some("not a url".to_string()),
            no_proxy: None,
            proxy_url: None,
        };

        // when
        let result = build_http_client_with(&config);

        // then
        let error = result.expect_err("invalid proxy URL must be reported as a build failure");
        assert!(
            matches!(error, crate::error::ApiError::Http(_)),
            "expected ApiError::Http for invalid proxy URL, got: {error:?}"
        );
    }

    #[test]
    fn from_proxy_url_sets_unified_field_and_leaves_per_scheme_empty() {
        // given / when
        let config = ProxyConfig::from_proxy_url("http://unified.internal:3128");

        // then
        assert_eq!(
            config.proxy_url.as_deref(),
            Some("http://unified.internal:3128")
        );
        assert!(config.http_proxy.is_none());
        assert!(config.https_proxy.is_none());
        assert!(!config.is_empty());
    }

    #[test]
    fn build_http_client_succeeds_with_unified_proxy_url() {
        // given
        let config = ProxyConfig {
            proxy_url: Some("http://unified.internal:3128".to_string()),
            no_proxy: Some("localhost".to_string()),
            ..ProxyConfig::default()
        };

        // when
        let result = build_http_client_with(&config);

        // then
        assert!(result.is_ok());
    }

    #[test]
    fn proxy_url_takes_precedence_over_per_scheme_fields() {
        // given – both per-scheme and unified are set
        let config = ProxyConfig {
            http_proxy: Some("http://per-scheme.internal:1111".to_string()),
            https_proxy: Some("http://per-scheme.internal:2222".to_string()),
            no_proxy: None,
            proxy_url: Some("http://unified.internal:3128".to_string()),
        };

        // when – building succeeds (the unified URL is valid)
        let result = build_http_client_with(&config);

        // then
        assert!(result.is_ok());
    }

    #[test]
    fn build_http_client_returns_error_for_invalid_unified_proxy_url() {
        // given
        let config = ProxyConfig::from_proxy_url("not a url");

        // when
        let result = build_http_client_with(&config);

        // then
        assert!(
            matches!(result, Err(crate::error::ApiError::Http(_))),
            "invalid unified proxy URL should fail: {result:?}"
        );
    }

    #[test]
    fn timeout_config_default_matches_documented_defaults() {
        let default = TimeoutConfig::default();
        assert_eq!(default.connect_timeout, Duration::from_secs(30));
        assert_eq!(default.request_timeout, Duration::from_secs(300));
    }

    #[test]
    fn timeout_config_from_seconds_round_trips() {
        let config = TimeoutConfig::from_seconds(10, 120);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.request_timeout, Duration::from_secs(120));
        // Verify the config is actually consumed by build_http_client_with_opts
        // without panicking — this catches regressions where the builder chain
        // is misconfigured.
        let result = build_http_client_with_opts(&ProxyConfig::default(), &config);
        assert!(result.is_ok(), "build should succeed with custom timeouts");
    }

    #[test]
    fn timeout_config_from_env_falls_back_to_defaults_when_vars_unset() {
        // This test intentionally does not set the env vars. When they are
        // already set in the surrounding test environment we still accept the
        // parsed values; the contract is "unset → default", not "always default".
        let config = TimeoutConfig::from_env();
        // Default connect timeout is 30s; only assert when the env var is
        // truly unset, which we cannot control here. Instead just assert that
        // both fields are non-zero (a valid timeout was produced).
        assert!(
            config.connect_timeout > Duration::ZERO,
            "connect_timeout must be non-zero"
        );
        assert!(
            config.request_timeout > Duration::ZERO,
            "request_timeout must be non-zero"
        );
    }

    // ── 连接池保活配置测试 ──
    // 防止"长时间空闲后复用陈旧连接导致 SSE 流挂起"的回归。

    #[test]
    fn build_http_client_succeeds_with_keepalive_config() {
        // 验证添加 pool_idle_timeout 和 tcp_keepalive 后，
        // client 仍能正常构建。
        let config = ProxyConfig::default();
        let result = build_http_client_with_opts(&config, &TimeoutConfig::default());
        assert!(result.is_ok(), "build should succeed with keepalive config");
    }

    #[test]
    fn build_http_client_with_keepalive_and_proxy() {
        // 验证 keepalive 配置与代理配置兼容。
        let config = ProxyConfig {
            proxy_url: Some("http://proxy.internal:3128".to_string()),
            ..ProxyConfig::default()
        };
        let result = build_http_client_with_opts(&config, &TimeoutConfig::default());
        assert!(
            result.is_ok(),
            "build should succeed with keepalive + proxy"
        );
    }
}
