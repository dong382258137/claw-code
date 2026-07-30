//! Integration tests for `ProviderClient` provider routing in the DeepSeek-only
//! build.
//!
//! Originally these tests asserted that the legacy enum-based `ProviderClient`
//! dispatched `grok` / `claude` models to xAI / Anthropic variants and that
//! `read_xai_base_url` honoured `XAI_BASE_URL`. After the DeepSeek-only
//! migration the only supported provider is DeepSeek, so the tests now verify
//! that:
//!   * `ProviderClient::from_model` resolves DeepSeek aliases.
//!   * `ProviderClient::from_model` surfaces `MissingCredentials` when
//!     `DEEPSEEK_API_KEY` is absent.
//!   * `read_base_url` honours `DEEPSEEK_BASE_URL`.
//!
//! Tests that exercised Anthropic / xAI-only surfaces (`from_model_with_anthropic_auth`,
//! `AuthSource::ApiKey`, `ProviderKind::Anthropic` / `ProviderKind::Xai`) are
//! preserved below as `#[ignore]`d migration audit stubs.

use std::ffi::OsString;
use std::sync::{Mutex, OnceLock};

use api::{read_base_url, ApiError, ProviderClient, ProviderKind};

#[test]
fn provider_client_routes_deepseek_aliases_to_deepseek_kind() {
    let _lock = env_lock();
    let _api_key = EnvVarGuard::set("DEEPSEEK_API_KEY", Some("deepseek-test-key"));

    let client = ProviderClient::from_model("pro").expect("pro alias should resolve to DeepSeek");

    assert_eq!(client.provider_kind(), ProviderKind::DeepSeek);
}

#[test]
fn provider_client_reports_missing_deepseek_credentials_when_env_unset() {
    let _lock = env_lock();
    let _api_key = EnvVarGuard::set("DEEPSEEK_API_KEY", None);
    // Defensive: ensure no .env leak from the working directory influences the
    // lookup. The `from_env` path consults `dotenv_value("DEEPSEEK_API_KEY")`
    // as a fallback, so unsetting the env var alone is not always sufficient.
    let _previous_cwd = EnvVarGuard::set_cwd_for_dotenv_isolation();

    let error = ProviderClient::from_model("deepseek-v4-pro")
        .expect_err("deepseek requests without DEEPSEEK_API_KEY should fail fast");

    match error {
        ApiError::MissingCredentials {
            provider,
            env_vars,
            ..
        } => {
            assert_eq!(provider, "DeepSeek");
            assert_eq!(env_vars, &["DEEPSEEK_API_KEY"]);
        }
        other => panic!("expected missing DeepSeek credentials, got {other:?}"),
    }
}

#[test]
fn read_base_url_prefers_deepseek_env_override() {
    let _lock = env_lock();
    let _base_url = EnvVarGuard::set(
        "DEEPSEEK_BASE_URL",
        Some("https://example.deepseek.test/v1"),
    );

    assert_eq!(
        read_base_url(),
        "https://example.deepseek.test/v1"
    );
}

#[test]
#[ignore = "from_model_with_anthropic_auth + AuthSource::ApiKey + ProviderKind::Anthropic were removed in the DeepSeek-only migration. DeepSeek auth is resolved internally via DEEPSEEK_API_KEY / OpenAiCompatClient::from_env, so there is no separate AuthSource surface to test."]
fn provider_client_uses_explicit_anthropic_auth_without_env_lookup() {
    // Migration audit stub. The original test verified that
    // `ProviderClient::from_model_with_anthropic_auth("claude-sonnet-4-6",
    // Some(AuthSource::ApiKey("anthropic-test-key")))` would skip env lookup
    // and return a `ProviderKind::Anthropic` client. Both the constructor and
    // the enum variant no longer exist.
}

#[test]
#[ignore = "ProviderKind::Xai and OpenAiCompatConfig::xai() were removed in the DeepSeek-only migration. xAI provider dispatch (XAI_API_KEY, read_xai_base_url, ProviderClient::Xai(_)) is no longer reachable."]
fn provider_client_routes_grok_aliases_through_xai() {
    // Migration audit stub. The original test verified that
    // `ProviderClient::from_model("grok-mini")` resolved to
    // `ProviderClient::Xai(_)` with `ProviderKind::Xai`. With the migration
    // `detect_provider_kind` always returns `ProviderKind::DeepSeek` and the
    // xAI-specific code paths have been deleted.
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let original = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self { key, original }
    }

    /// Move the process cwd into a fresh temp directory so `dotenv_value`
    /// (which reads `./.env`) cannot accidentally supply `DEEPSEEK_API_KEY`.
    /// Returns a guard that restores the previous cwd on drop.
    fn set_cwd_for_dotenv_isolation() -> CwdGuard {
        let previous = std::env::current_dir().ok();
        let temp = std::env::temp_dir().join(format!(
            "api-provider-client-isolation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = std::fs::create_dir_all(&temp);
        let _ = std::env::set_current_dir(&temp);
        CwdGuard { previous, temp }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

struct CwdGuard {
    previous: Option<std::path::PathBuf>,
    temp: std::path::PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            let _ = std::env::set_current_dir(previous);
        }
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}
