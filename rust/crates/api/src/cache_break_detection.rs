//! # Cache Break Detection — 服务端前缀缓存命中率下降检测
//!
//! ## 背景
//!
//! DeepSeek 服务端 prefix cache 通过 `prompt_cache_hit_tokens` /
//! `prompt_cache_miss_tokens` 字段在每次 API 响应中返回命中信息。
//! 本模块**不缓存响应**(那是客户端 completion cache 的职责,已废弃),
//! 只做**命中率下降检测与原因分类**,供 `claw doctor --cache-stats` 诊断。
//!
//! ## 工作原理
//!
//! 每次请求记录 prompt 指纹(model/system/tools/messages 的 FNV-1a 哈希),
//! 与上次请求对比。当 `cache_read_input_tokens` 下降超过阈值时触发 break event,
//! 并按根因分类(model_changed / system_prompt_changed / tool_definitions_changed /
//! message_payload_changed / ttl_expiry / unknown)。
//!
//! 持久化到 `~/.claude/cache/prompt-cache/<session>/stats.json` +
//! `session-state.json`,跨 session 累计。
//!
//! ## 与旧 prompt_cache.rs 的区别
//!
//! - **移除** completion cache(`lookup_completion` / `record_response`)
//! - **保留** break detection + stats + 诊断报表
//! - **简化** 不再依赖 `MessageResponse`,只需 `MessageRequest` + `Usage`

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::types::{MessageRequest, Usage};

const DEFAULT_PROMPT_TTL_SECS: u64 = 5 * 60;
const DEFAULT_BREAK_MIN_DROP: u32 = 2_000;
const MAX_SANITIZED_LENGTH: usize = 80;
const REQUEST_FINGERPRINT_VERSION: u32 = 1;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Debug, Clone)]
pub struct CacheBreakConfig {
    pub session_id: String,
    pub prompt_ttl: Duration,
    pub cache_break_min_drop: u32,
}

impl CacheBreakConfig {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            prompt_ttl: Duration::from_secs(DEFAULT_PROMPT_TTL_SECS),
            cache_break_min_drop: DEFAULT_BREAK_MIN_DROP,
        }
    }
}

impl Default for CacheBreakConfig {
    fn default() -> Self {
        Self::new("default")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakPaths {
    pub root: PathBuf,
    pub session_dir: PathBuf,
    pub session_state_path: PathBuf,
    pub stats_path: PathBuf,
}

impl CacheBreakPaths {
    #[must_use]
    pub fn for_session(session_id: &str) -> Self {
        let root = base_cache_root();
        let session_dir = root.join(sanitize_path_segment(session_id));
        Self {
            root,
            session_state_path: session_dir.join("session-state.json"),
            stats_path: session_dir.join("stats.json"),
            session_dir,
        }
    }
}

/// Breakdown of cache break events by root cause.
///
/// `system_prompt_changed` / `tool_definitions_changed` 表示动态值泄漏到
/// 静态区,是 cache alignment 优化的候选。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakReasons {
    pub model_changed: u64,
    pub system_prompt_changed: u64,
    pub tool_definitions_changed: u64,
    pub message_payload_changed: u64,
    pub ttl_expiry: u64,
    pub unknown: u64,
}

impl CacheBreakReasons {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.model_changed
            + self.system_prompt_changed
            + self.tool_definitions_changed
            + self.message_payload_changed
            + self.ttl_expiry
            + self.unknown
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakStats {
    pub tracked_requests: u64,
    pub expected_invalidations: u64,
    pub unexpected_cache_breaks: u64,
    pub total_cache_creation_input_tokens: u64,
    pub total_cache_read_input_tokens: u64,
    pub last_cache_creation_input_tokens: Option<u32>,
    pub last_cache_read_input_tokens: Option<u32>,
    pub last_request_hash: Option<String>,
    pub last_break_reason: Option<String>,
    #[serde(default)]
    pub break_reasons: CacheBreakReasons,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakEvent {
    pub unexpected: bool,
    pub reason: String,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheBreakRecord {
    pub cache_break: Option<CacheBreakEvent>,
    pub stats: CacheBreakStats,
}

#[derive(Debug, Clone)]
pub struct CacheBreakDetector {
    inner: Arc<Mutex<CacheBreakInner>>,
}

#[derive(Debug)]
struct CacheBreakInner {
    config: CacheBreakConfig,
    paths: CacheBreakPaths,
    stats: CacheBreakStats,
    previous: Option<TrackedPromptState>,
}

impl CacheBreakDetector {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self::with_config(CacheBreakConfig::new(session_id))
    }

    #[must_use]
    pub fn with_config(config: CacheBreakConfig) -> Self {
        let paths = CacheBreakPaths::for_session(&config.session_id);
        let stats = read_json::<CacheBreakStats>(&paths.stats_path).unwrap_or_default();
        let previous = read_json::<TrackedPromptState>(&paths.session_state_path);
        Self {
            inner: Arc::new(Mutex::new(CacheBreakInner {
                config,
                paths,
                stats,
                previous,
            })),
        }
    }

    #[must_use]
    pub fn paths(&self) -> CacheBreakPaths {
        self.lock().paths.clone()
    }

    #[must_use]
    pub fn stats(&self) -> CacheBreakStats {
        self.lock().stats.clone()
    }

    /// 记录一次 API 响应的 usage,检测 cache break。
    ///
    /// 在 streaming.rs / response_to_events 收到 usage 后调用。
    /// 不缓存响应,只更新指纹状态和统计。
    #[must_use]
    pub fn record_usage(&self, request: &MessageRequest, usage: &Usage) -> CacheBreakRecord {
        let request_hash = request_hash_hex(request);
        let mut inner = self.lock();
        let previous = inner.previous.clone();
        let current = TrackedPromptState::from_usage(request, usage);
        let cache_break = detect_cache_break(&inner.config, previous.as_ref(), &current);

        inner.stats.tracked_requests += 1;
        apply_usage_to_stats(&mut inner.stats, usage, &request_hash);
        if let Some(event) = &cache_break {
            if event.unexpected {
                inner.stats.unexpected_cache_breaks += 1;
            } else {
                inner.stats.expected_invalidations += 1;
            }
            inner.stats.last_break_reason = Some(event.reason.clone());
            classify_break_reason(&event.reason, &mut inner.stats.break_reasons);
        }

        inner.previous = Some(current);
        persist_state(&inner);

        CacheBreakRecord {
            cache_break,
            stats: inner.stats.clone(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheBreakInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TrackedPromptState {
    observed_at_unix_secs: u64,
    #[serde(default = "current_fingerprint_version")]
    fingerprint_version: u32,
    model_hash: u64,
    system_hash: u64,
    tools_hash: u64,
    messages_hash: u64,
    cache_read_input_tokens: u32,
}

impl TrackedPromptState {
    fn from_usage(request: &MessageRequest, usage: &Usage) -> Self {
        let hashes = RequestFingerprints::from_request(request);
        Self {
            observed_at_unix_secs: now_unix_secs(),
            fingerprint_version: current_fingerprint_version(),
            model_hash: hashes.model,
            system_hash: hashes.system,
            tools_hash: hashes.tools,
            messages_hash: hashes.messages,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestFingerprints {
    model: u64,
    system: u64,
    tools: u64,
    messages: u64,
}

impl RequestFingerprints {
    fn from_request(request: &MessageRequest) -> Self {
        Self {
            model: hash_serializable(&request.model),
            system: hash_serializable(&request.system),
            tools: hash_serializable(&request.tools),
            messages: hash_serializable(&request.messages),
        }
    }
}

fn detect_cache_break(
    config: &CacheBreakConfig,
    previous: Option<&TrackedPromptState>,
    current: &TrackedPromptState,
) -> Option<CacheBreakEvent> {
    let previous = previous?;
    if previous.fingerprint_version != current.fingerprint_version {
        return Some(CacheBreakEvent {
            unexpected: false,
            reason: format!(
                "fingerprint version changed (v{} -> v{})",
                previous.fingerprint_version, current.fingerprint_version
            ),
            previous_cache_read_input_tokens: previous.cache_read_input_tokens,
            current_cache_read_input_tokens: current.cache_read_input_tokens,
            token_drop: previous
                .cache_read_input_tokens
                .saturating_sub(current.cache_read_input_tokens),
        });
    }
    let token_drop = previous
        .cache_read_input_tokens
        .saturating_sub(current.cache_read_input_tokens);
    if token_drop < config.cache_break_min_drop {
        return None;
    }

    let mut reasons = Vec::new();
    if previous.model_hash != current.model_hash {
        reasons.push("model changed");
    }
    if previous.system_hash != current.system_hash {
        reasons.push("system prompt changed");
    }
    if previous.tools_hash != current.tools_hash {
        reasons.push("tool definitions changed");
    }
    if previous.messages_hash != current.messages_hash {
        reasons.push("message payload changed");
    }

    let elapsed = current
        .observed_at_unix_secs
        .saturating_sub(previous.observed_at_unix_secs);

    let (unexpected, reason) = if reasons.is_empty() {
        if elapsed > config.prompt_ttl.as_secs() {
            (
                false,
                format!("possible prompt cache TTL expiry after {elapsed}s"),
            )
        } else {
            (
                true,
                "cache read tokens dropped while prompt fingerprint remained stable".to_string(),
            )
        }
    } else {
        (false, reasons.join(", "))
    };

    Some(CacheBreakEvent {
        unexpected,
        reason,
        previous_cache_read_input_tokens: previous.cache_read_input_tokens,
        current_cache_read_input_tokens: current.cache_read_input_tokens,
        token_drop,
    })
}

/// 按根因分类 break reason 字符串,累加到对应计数器。
fn classify_break_reason(reason: &str, reasons: &mut CacheBreakReasons) {
    if reason.contains("model changed") {
        reasons.model_changed += 1;
    }
    if reason.contains("system prompt changed") {
        reasons.system_prompt_changed += 1;
    }
    if reason.contains("tool definitions changed") {
        reasons.tool_definitions_changed += 1;
    }
    if reason.contains("message payload changed") {
        reasons.message_payload_changed += 1;
    }
    if reason.contains("TTL") || reason.contains("expir") {
        reasons.ttl_expiry += 1;
    }
    if reason.contains("stable") {
        reasons.unknown += 1;
    }
}

fn apply_usage_to_stats(
    stats: &mut CacheBreakStats,
    usage: &Usage,
    request_hash: &str,
) {
    stats.total_cache_creation_input_tokens += u64::from(usage.cache_creation_input_tokens);
    stats.total_cache_read_input_tokens += u64::from(usage.cache_read_input_tokens);
    stats.last_cache_creation_input_tokens = Some(usage.cache_creation_input_tokens);
    stats.last_cache_read_input_tokens = Some(usage.cache_read_input_tokens);
    stats.last_request_hash = Some(request_hash.to_string());
}

fn persist_state(inner: &CacheBreakInner) {
    let _ = ensure_cache_dirs(&inner.paths);
    let _ = write_json(&inner.paths.stats_path, &inner.stats);
    if let Some(previous) = &inner.previous {
        let _ = write_json(&inner.paths.session_state_path, previous);
    }
}

fn ensure_cache_dirs(paths: &CacheBreakPaths) -> std::io::Result<()> {
    fs::create_dir_all(&paths.session_dir)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    fs::write(path, json)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn request_hash_hex(request: &MessageRequest) -> String {
    format!(
        "v{REQUEST_FINGERPRINT_VERSION}-{:016x}",
        hash_serializable(request)
    )
}

fn hash_serializable<T: Serialize>(value: &T) -> u64 {
    let json = serde_json::to_vec(value).unwrap_or_default();
    stable_hash_bytes(&json)
}

fn sanitize_path_segment(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let suffix = format!("-{:x}", hash_string(value));
    format!(
        "{}{}",
        &sanitized[..MAX_SANITIZED_LENGTH.saturating_sub(suffix.len())],
        suffix
    )
}

fn hash_string(value: &str) -> u64 {
    stable_hash_bytes(value.as_bytes())
}

/// 返回 cache break stats 的根目录。
///
/// 环境变量优先级:`CLAUDE_CONFIG_HOME` > `USERPROFILE`(Windows) >
/// `HOME`(Unix) > 系统临时目录(最后兜底)。
#[must_use]
pub fn cache_break_root() -> PathBuf {
    if let Some(config_home) = std::env::var_os("CLAUDE_CONFIG_HOME") {
        return PathBuf::from(config_home)
            .join("cache")
            .join("prompt-cache");
    }
    // Windows: USERPROFILE; Unix: HOME
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    if let Some(home) = home {
        return PathBuf::from(home)
            .join(".claude")
            .join("cache")
            .join("prompt-cache");
    }
    std::env::temp_dir().join("claude-prompt-cache")
}

fn base_cache_root() -> PathBuf {
    cache_break_root()
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

const fn current_fingerprint_version() -> u32 {
    REQUEST_FINGERPRINT_VERSION
}

fn stable_hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        detect_cache_break, read_json, request_hash_hex, sanitize_path_segment,
        CacheBreakConfig, CacheBreakDetector, CacheBreakPaths, TrackedPromptState,
    };
    use crate::types::{
        InputMessage, MessageRequest, SystemContent, Usage,
    };

    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn path_builder_sanitizes_session_identifier() {
        let paths = CacheBreakPaths::for_session("session:/with spaces");
        let session_dir = paths
            .session_dir
            .file_name()
            .and_then(|value| value.to_str())
            .expect("session dir name");
        assert_eq!(session_dir, "session--with-spaces");
        assert!(paths.stats_path.ends_with("stats.json"));
        assert!(paths.session_state_path.ends_with("session-state.json"));
    }

    #[test]
    fn request_fingerprint_drives_unexpected_break_detection() {
        let request = sample_request("same");
        let previous = TrackedPromptState::from_usage(
            &request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
            },
        );
        let current = TrackedPromptState::from_usage(
            &request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
            },
        );
        let event = detect_cache_break(&CacheBreakConfig::default(), Some(&previous), &current)
            .expect("break should be detected");
        assert!(event.unexpected);
        assert!(event.reason.contains("stable"));
    }

    #[test]
    fn changed_prompt_marks_break_as_expected() {
        let previous_request = sample_request("first");
        let current_request = sample_request("second");
        let previous = TrackedPromptState::from_usage(
            &previous_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
            },
        );
        let current = TrackedPromptState::from_usage(
            &current_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
            },
        );
        let event = detect_cache_break(&CacheBreakConfig::default(), Some(&previous), &current)
            .expect("break should be detected");
        assert!(!event.unexpected);
        assert!(event.reason.contains("message payload changed"));
    }

    #[test]
    fn record_usage_persists_state_and_detects_break() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "cache-break-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        let detector = CacheBreakDetector::new("unit-test-session");
        let request = sample_request("first turn");
        let usage = Usage {
            input_tokens: 0,
            cache_creation_input_tokens: 5_000,
            cache_read_input_tokens: 45_000,
            output_tokens: 100,
        };
        let record = detector.record_usage(&request, &usage);
        assert!(record.cache_break.is_none());
        assert_eq!(record.stats.tracked_requests, 1);

        // 第二次请求:system prompt 改变 + 命中率下降
        let request2 = sample_request_with_system("second turn", "changed system");
        let usage2 = Usage {
            input_tokens: 0,
            cache_creation_input_tokens: 40_000,
            cache_read_input_tokens: 10_000,
            output_tokens: 100,
        };
        let record2 = detector.record_usage(&request2, &usage2);
        let event = record2.cache_break.expect("break should be detected");
        assert!(!event.unexpected);
        assert!(event.reason.contains("system prompt changed"));

        let persisted = read_json::<super::CacheBreakStats>(&detector.paths().stats_path)
            .expect("stats should persist");
        assert_eq!(persisted.tracked_requests, 2);
        assert_eq!(persisted.expected_invalidations, 1);
        assert_eq!(persisted.break_reasons.system_prompt_changed, 1);

        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }

    #[test]
    fn ttl_expiry_marks_break_as_expected() {
        let request = sample_request("same");
        let mut previous = TrackedPromptState::from_usage(
            &request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
            },
        );
        // 模拟 10 分钟前
        previous.observed_at_unix_secs = previous.observed_at_unix_secs.saturating_sub(600);
        let current = TrackedPromptState::from_usage(
            &request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
            },
        );
        let event = detect_cache_break(&CacheBreakConfig::default(), Some(&previous), &current)
            .expect("break should be detected");
        assert!(!event.unexpected);
        assert!(event.reason.contains("TTL"));
    }

    #[test]
    fn sanitize_path_caps_long_values() {
        let long_value = "x".repeat(200);
        let sanitized = sanitize_path_segment(&long_value);
        assert!(sanitized.len() <= 80);
    }

    #[test]
    fn request_hashes_are_versioned_and_stable() {
        let request = sample_request("stable");
        let first = request_hash_hex(&request);
        let second = request_hash_hex(&request);
        assert_eq!(first, second);
        assert!(first.starts_with('v'));
    }

    fn sample_request(text: &str) -> MessageRequest {
        MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 64,
            messages: vec![InputMessage::user_text(text)],
            system: Some(SystemContent::from_text("system")),
            ..Default::default()
        }
    }

    fn sample_request_with_system(text: &str, system: &str) -> MessageRequest {
        MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 64,
            messages: vec![InputMessage::user_text(text)],
            system: Some(SystemContent::from_text(system)),
            ..Default::default()
        }
    }
}
