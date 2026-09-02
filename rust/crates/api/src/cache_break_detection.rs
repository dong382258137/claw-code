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

use crate::types::{MessageRequest, SystemContent, Usage};

const DEFAULT_PROMPT_TTL_SECS: u64 = 5 * 60;
const DEFAULT_BREAK_MIN_DROP: u32 = 2_000;
const MAX_SANITIZED_LENGTH: usize = 80;
// v2:system 指纹从"整段哈希"改为"仅静态前缀哈希"(`system_hash` 语义变更),
// 并新增 `system_full_hash` 单独度量动态段 churn。旧版本持久化的指纹语义
// 已不兼容,故递增版本号以干净地失效旧 session-state。
const REQUEST_FINGERPRINT_VERSION: u32 = 2;
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
    /// 请求侧静态前缀指纹持久化(独立于响应侧 `session_state_path`)。
    ///
    /// 请求侧 `note_request` 只记录 model/system_static/tools 指纹,不触碰
    /// 响应侧 `previous` 状态槽,两条检测链路互不干扰。
    pub request_side_state_path: PathBuf,
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
            request_side_state_path: session_dir.join("request-prefix-state.json"),
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
    /// 静态 system 前缀未变,但整段 system(含动态段:memory/goal/git/plan)已变。
    /// 这是每 turn 注入 volatile 上下文的预期 churn,不是静态区被污染。
    #[serde(default)]
    pub dynamic_section_changed: u64,
    /// 请求侧静态前缀漂移计数(P0-1 Pre-flight Guard)。
    ///
    /// 由 `note_request` 在请求**发出前**比对上一轮 model/system_static/tools
    /// 指纹得出 —— 任何未来代码把动态值(时间戳/UUID/路径/状态)写进静态前缀,
    /// 会在首个 turn 就被此计数捕获,而非等命中率曲线慢慢暴露。
    #[serde(default)]
    pub prefix_drifted: u64,
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
            + self.dynamic_section_changed
            + self.prefix_drifted
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

#[derive(Debug, Clone)]
struct CacheBreakInner {
    config: CacheBreakConfig,
    paths: CacheBreakPaths,
    stats: CacheBreakStats,
    previous: Option<TrackedPromptState>,
    /// 请求侧静态前缀指纹(上一轮)。与响应侧 `previous` 独立维护:
    /// `note_request` 只更新这里,`record_usage` 只更新 `previous`。
    previous_request_side: Option<RequestSidePrefix>,
}

/// 请求侧静态前缀指纹(P0-1 Pre-flight Guard)。
///
/// 只记录会破坏前缀缓存的三个维度:model / 静态 system / tools。
/// `messages` 数组增长是多轮/tool-loop 的预期行为,不参与比对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RequestSidePrefix {
    model_hash: u64,
    system_hash: u64,
    tools_hash: u64,
    observed_at_unix_secs: u64,
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
        let previous_request_side = read_json::<RequestSidePrefix>(&paths.request_side_state_path);
        Self {
            inner: Arc::new(Mutex::new(CacheBreakInner {
                config,
                paths,
                stats,
                previous,
                previous_request_side,
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
        self.record_usage_inner(request, usage, /* multi_turn = */ false)
    }

    /// 多轮 tool call 循环专用:仅比对 system/tools/model hash,忽略 messages_hash 变化。
    ///
    /// 用于子智能体多轮 tool call 循环(设计文档 §3.3.3):多轮中 system prompt 不变,
    /// `messages` 数组增长是预期行为,不应触发 "message payload changed" break 误报。
    /// 主 agent 的 `CacheBreakDetector` 仍用 [`record_usage`](Self::record_usage)
    /// (主 agent 的 messages 增长确实是 break 信号)。
    #[must_use]
    pub fn record_usage_multi_turn(
        &self,
        request: &MessageRequest,
        usage: &Usage,
    ) -> CacheBreakRecord {
        self.record_usage_inner(request, usage, /* multi_turn = */ true)
    }

    /// 请求侧静态前缀不变式断言(P0-1 Pre-flight Guard)。
    ///
    /// 在请求**发出前**调用:比对上一轮 model / 静态 system / tools 指纹。
    /// 若静态前缀意外漂移(未来代码把动态值写进静态区、工具定义中途变化),
    /// 返回漂移原因并累加 `break_reasons.prefix_drifted` 计数(供
    /// `claw doctor --cache-stats` 诊断),不触发任何请求侧行为变更。
    ///
    /// `messages` 数组增长不参与比对(多轮/tool-loop 预期行为)。
    /// 独立维护 `previous_request_side` 状态槽,与响应侧 `record_usage`
    /// 的 `previous` 互不干扰。首次调用(`previous_request_side` 为 None)
    /// 仅记录基线,不告警。
    #[must_use]
    pub fn note_request(&self, request: &MessageRequest) -> Option<String> {
        let hashes = RequestFingerprints::from_request(request);
        let current = RequestSidePrefix {
            model_hash: hashes.model,
            system_hash: hashes.system_static,
            tools_hash: hashes.tools,
            observed_at_unix_secs: now_unix_secs(),
        };

        let mut inner = self.lock();
        let drift = match &inner.previous_request_side {
            None => None,
            Some(prev) => {
                let mut reasons = Vec::new();
                if prev.model_hash != current.model_hash {
                    reasons.push("model changed");
                }
                if prev.system_hash != current.system_hash {
                    reasons.push("system prompt changed");
                }
                if prev.tools_hash != current.tools_hash {
                    reasons.push("tool definitions changed");
                }
                if reasons.is_empty() {
                    None
                } else {
                    Some(reasons.join(", "))
                }
            }
        };

        if let Some(reason) = &drift {
            inner.stats.break_reasons.prefix_drifted += 1;
            inner.stats.last_break_reason = Some(reason.clone());
        }
        inner.previous_request_side = Some(current);
        persist_request_side(&inner);
        drift
    }

    /// `record_usage` / `record_usage_multi_turn` 共用实现。
    ///
    /// `multi_turn=true` 时调用 [`detect_cache_break_multi_turn`],跳过 messages_hash 比对。
    fn record_usage_inner(
        &self,
        request: &MessageRequest,
        usage: &Usage,
        multi_turn: bool,
    ) -> CacheBreakRecord {
        let request_hash = request_hash_hex(request);
        let mut inner = self.lock();
        let previous = inner.previous.clone();
        let current = TrackedPromptState::from_usage(request, usage);
        let cache_break = if multi_turn {
            detect_cache_break_multi_turn(&inner.config, previous.as_ref(), &current)
        } else {
            detect_cache_break(&inner.config, previous.as_ref(), &current)
        };

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
    /// 静态 system 前缀哈希(仅带 cache_control 的块)。动态段 churn 不改变它。
    system_hash: u64,
    /// 整段 system(含动态段)哈希,用于单独量化动态段 churn。
    #[serde(default)]
    system_full_hash: u64,
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
            system_hash: hashes.system_static,
            system_full_hash: hashes.system_full,
            tools_hash: hashes.tools,
            messages_hash: hashes.messages,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestFingerprints {
    model: u64,
    system_static: u64,
    system_full: u64,
    tools: u64,
    messages: u64,
}

impl RequestFingerprints {
    fn from_request(request: &MessageRequest) -> Self {
        Self {
            model: hash_serializable(&request.model),
            system_static: hash_static_system(&request.system),
            system_full: hash_serializable(&request.system),
            tools: hash_serializable(&request.tools),
            messages: hash_serializable(&request.messages),
        }
    }
}

/// 仅对 system 的静态(可缓存)前缀取哈希。
///
/// [`build_system_blocks`] 保证静态段在前(断点携带 `cache_control`),动态段
/// 在后(不携带 marker)。因此静态前缀 = 直到最后一个带 cache marker 的块为止。
/// volatile 动态段(memory/goal/git/plan)的 churn 不会改变此哈希——只有真正的
/// 静态指令变化才会。无 cache marker 时(如 legacy `Text` 形式)回退到整段哈希。
fn hash_static_system(system: &Option<SystemContent>) -> u64 {
    match system {
        Some(SystemContent::Blocks(blocks)) => {
            match blocks.iter().rposition(|b| b.cache_control.is_some()) {
                Some(end) => hash_serializable(&blocks[..=end]),
                None => hash_serializable(system),
            }
        }
        _ => hash_serializable(system),
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
    // 静态前缀未变但整段 system(含动态段:memory/goal/git/plan)已变:
    // 这是每 turn 注入 volatile 上下文的预期 churn,单独归因以便量化。
    if previous.system_hash == current.system_hash
        && previous.system_full_hash != current.system_full_hash
    {
        reasons.push("dynamic section changed");
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

/// 多轮 tool call 循环专用 cache break 检测(设计文档 §3.3.3)。
///
/// 与 [`detect_cache_break`] 的差异:**不检查 `messages_hash`**。
/// 多轮循环中 system prompt 不变,`messages` 数组增长(追加 assistant + tool_result)
/// 是预期行为,前缀缓存命中是正常的;只有 system/tools/model hash 变化
/// (如 capability 切换)才视为真实 break。
fn detect_cache_break_multi_turn(
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

    // ⚠ 不检查 messages_hash — 多轮循环中 messages 增长是预期行为
    let mut reasons = Vec::new();
    if previous.model_hash != current.model_hash {
        reasons.push("model changed");
    }
    if previous.system_hash != current.system_hash {
        reasons.push("system prompt changed");
    }
    // 静态前缀未变但整段 system(含动态段)已变:预期 churn,单独归因。
    if previous.system_hash == current.system_hash
        && previous.system_full_hash != current.system_full_hash
    {
        reasons.push("dynamic section changed");
    }
    if previous.tools_hash != current.tools_hash {
        reasons.push("tool definitions changed");
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
    if reason.contains("dynamic section changed") {
        reasons.dynamic_section_changed += 1;
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

fn apply_usage_to_stats(stats: &mut CacheBreakStats, usage: &Usage, request_hash: &str) {
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

fn persist_request_side(inner: &CacheBreakInner) {
    let _ = ensure_cache_dirs(&inner.paths);
    let _ = write_json(&inner.paths.stats_path, &inner.stats);
    if let Some(previous) = &inner.previous_request_side {
        let _ = write_json(&inner.paths.request_side_state_path, previous);
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

fn hash_serializable<T: Serialize + ?Sized>(value: &T) -> u64 {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        detect_cache_break, read_json, request_hash_hex, sanitize_path_segment, CacheBreakConfig,
        CacheBreakDetector, CacheBreakPaths, TrackedPromptState,
    };
    use crate::types::{
        CacheControl, InputMessage, MessageRequest, SystemBlock, SystemContent, Usage,
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

    /// §3.3.3:多轮循环中 messages 增长但 system/tools/model 不变,且 token 下降。
    /// 普通 `detect_cache_break` 会把下降归因于 "message payload changed"
    /// (多轮中 messages 必然增长,该归因是噪声);`detect_cache_break_multi_turn`
    /// 跳过 messages_hash 比对,**不产生 "message payload changed" 归因**。
    #[test]
    fn multi_turn_does_not_attribute_drop_to_messages_change() {
        let prev_request = sample_request("turn 1");
        // 同一 system("system")/tools/model,仅 messages 不同(模拟 tool_result 追加)
        let curr_request = sample_request_with_system("turn 2", "system");
        let previous = TrackedPromptState::from_usage(
            &prev_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
            },
        );
        let current = TrackedPromptState::from_usage(
            &curr_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000, // drop=5000 ≥ min_drop(2000)
                output_tokens: 0,
            },
        );
        // 普通 detect_cache_break:messages_hash 变化 → 归因 "message payload changed"
        let normal = detect_cache_break(&CacheBreakConfig::default(), Some(&previous), &current)
            .expect("normal detector flags the drop");
        assert!(!normal.unexpected);
        assert!(
            normal.reason.contains("message payload changed"),
            "normal detector attributes drop to messages change: {}",
            normal.reason
        );

        // multi-turn:跳过 messages_hash → 不归因 "message payload changed"
        let multi = super::detect_cache_break_multi_turn(
            &CacheBreakConfig::default(),
            Some(&previous),
            &current,
        )
        .expect("multi-turn still surfaces the unexplained drop");
        assert!(
            !multi.reason.contains("message payload changed"),
            "multi-turn must NOT attribute drop to messages change: {}",
            multi.reason
        );
    }

    /// §3.3.3:多轮循环中 system prompt 变化(如 capability 切换)→
    /// `detect_cache_break_multi_turn` 应归因到 "system prompt changed"(真实失效),
    /// 而非 "message payload changed"。
    #[test]
    fn multi_turn_flags_system_prompt_change() {
        let prev_request = sample_request_with_system("turn 1", "system-A");
        let curr_request = sample_request_with_system("turn 2", "system-B");
        let previous = TrackedPromptState::from_usage(
            &prev_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
            },
        );
        let current = TrackedPromptState::from_usage(
            &curr_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
            },
        );
        let event = super::detect_cache_break_multi_turn(
            &CacheBreakConfig::default(),
            Some(&previous),
            &current,
        )
        .expect("system prompt change should be flagged even in multi-turn");
        assert!(!event.unexpected);
        assert!(event.reason.contains("system prompt changed"));
        assert!(!event.reason.contains("message payload changed"));
    }

    /// §3.3.3:多轮循环中前缀缓存命中(token 无下降)→ multi-turn 不触发 break
    /// (与普通检测器一致;此为多轮循环的常态)。
    #[test]
    fn multi_turn_no_break_when_cache_hits() {
        let prev_request = sample_request("turn 1");
        let curr_request = sample_request_with_system("turn 2", "system");
        let previous = TrackedPromptState::from_usage(
            &prev_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 45_000,
                output_tokens: 0,
            },
        );
        let current = TrackedPromptState::from_usage(
            &curr_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 45_000, // drop=0 < min_drop → 无 break
                output_tokens: 0,
            },
        );
        assert!(
            detect_cache_break(&CacheBreakConfig::default(), Some(&previous), &current).is_none()
        );
        assert!(
            super::detect_cache_break_multi_turn(
                &CacheBreakConfig::default(),
                Some(&previous),
                &current,
            )
            .is_none(),
            "no drop → no break even in multi-turn"
        );
    }

    /// 构造带 [静态块(带 cache_control), 动态块(无 marker)] 的请求。
    fn request_with_split_system(
        static_text: &str,
        dynamic_text: &str,
        msg: &str,
    ) -> MessageRequest {
        MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            max_tokens: 64,
            messages: vec![InputMessage::user_text(msg)],
            system: Some(SystemContent::Blocks(vec![
                SystemBlock::new(static_text).with_cache_control(CacheControl::ephemeral()),
                SystemBlock::new(dynamic_text),
            ])),
            ..Default::default()
        }
    }

    fn throughput_request(state: &TrackedPromptState) -> TrackedPromptState {
        // 由已构造的 state 派生同构 state:cpu 换 usage 无需重算,直接建同 shape。
        state.clone()
    }

    /// 仅动态段(无 cache_control 的尾部块)变化 → 静态前缀未变。
    /// 不应归因 "system prompt changed",而应归因 "dynamic section changed"(预期 churn)。
    #[test]
    fn dynamic_section_churn_is_not_attributed_to_static_system_change() {
        let prev_request = request_with_split_system("STATIC", "mem-v1", "turn 1");
        let curr_request = request_with_split_system("STATIC", "mem-v2", "turn 2");
        let previous = TrackedPromptState::from_usage(
            &prev_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
            },
        );
        let current = TrackedPromptState::from_usage(
            &curr_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
            },
        );
        let event = detect_cache_break(&CacheBreakConfig::default(), Some(&previous), &current)
            .expect("drop should be surfaced");
        assert!(!event.unexpected);
        assert!(
            !event.reason.contains("system prompt changed"),
            "dynamic churn must not be attributed to static system change: {}",
            event.reason
        );
        assert!(
            event.reason.contains("dynamic section changed"),
            "expected dynamic section changed, got: {}",
            event.reason
        );
    }

    /// 静态块(带 cache_control)变化 → 仍应归因 "system prompt changed"(真实静态破坏)。
    #[test]
    fn static_system_change_still_attributed_to_system_prompt_change() {
        let prev_request = request_with_split_system("STATIC-v1", "mem", "turn 1");
        let curr_request = request_with_split_system("STATIC-v2", "mem", "turn 2");
        let previous = TrackedPromptState::from_usage(
            &prev_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
            },
        );
        let current = TrackedPromptState::from_usage(
            &curr_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
            },
        );
        let event = detect_cache_break(&CacheBreakConfig::default(), Some(&previous), &current)
            .expect("static change should be surfaced");
        assert!(!event.unexpected);
        assert!(
            event.reason.contains("system prompt changed"),
            "static change must be attributed to system prompt: {}",
            event.reason
        );
    }

    /// multi_turn 检测器同样能区分动态段 churn(预期)与静态破坏。
    #[test]
    fn multi_turn_flags_dynamic_section_churn_not_static_change() {
        let prev_request = request_with_split_system("STATIC", "mem-v1", "turn 1");
        let curr_request = request_with_split_system("STATIC", "mem-v2", "turn 2");
        let previous = TrackedPromptState::from_usage(
            &prev_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
            },
        );
        let current = TrackedPromptState::from_usage(
            &curr_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
            },
        );
        let event = super::detect_cache_break_multi_turn(
            &CacheBreakConfig::default(),
            Some(&previous),
            &current,
        )
        .expect("dynamic churn should be surfaced in multi-turn");
        assert!(
            !event.reason.contains("system prompt changed"),
            "dynamic churn must not be attributed to static system change: {}",
            event.reason
        );
        assert!(
            event.reason.contains("dynamic section changed"),
            "expected dynamic section changed, got: {}",
            event.reason
        );
    }

    /// `record_usage` 落盘后 `break_reasons.dynamic_section_changed` 计数正确。
    #[test]
    fn record_usage_counts_dynamic_section_churn() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "cache-break-dyn-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        let detector = CacheBreakDetector::new("unit-test-dynamic");
        let request = request_with_split_system("STATIC", "mem-v1", "first turn");
        let usage = Usage {
            input_tokens: 0,
            cache_creation_input_tokens: 5_000,
            cache_read_input_tokens: 45_000,
            output_tokens: 100,
        };
        let _ = detector.record_usage(&request, &usage);

        // 同静态,动态段变化 + 命中率下降
        let request2 = request_with_split_system("STATIC", "mem-v2", "second turn");
        let usage2 = Usage {
            input_tokens: 0,
            cache_creation_input_tokens: 40_000,
            cache_read_input_tokens: 10_000,
            output_tokens: 100,
        };
        let record2 = detector.record_usage(&request2, &usage2);
        let event = record2.cache_break.expect("break should be detected");
        assert!(
            event.reason.contains("dynamic section changed"),
            "reason: {}",
            event.reason
        );
        assert_eq!(record2.stats.break_reasons.dynamic_section_changed, 1);
        assert_eq!(record2.stats.break_reasons.system_prompt_changed, 0);

        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }

    /// 为 P0-1 测试隔离磁盘状态:临时 CLAUDE_CONFIG_HOME 根,测试后清理。
    fn prefix_test_env(guard_tag: &str) -> (std::sync::MutexGuard<'static, ()>, std::path::PathBuf) {
        let guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "cache-break-prefix-{guard_tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        (guard, temp_root)
    }

    fn prefix_test_cleanup(temp_root: std::path::PathBuf) {
        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }

    /// P0-1 Pre-flight Guard:首次 note_request 仅建基线,不告警。
    #[test]
    fn note_request_first_call_establishes_baseline_without_alert() {
        let (_guard, temp_root) = prefix_test_env("first");
        let request = request_with_split_system("STATIC", "mem", "turn 1");
        let detector = CacheBreakDetector::new("unit-test-prefix");
        assert!(detector.note_request(&request).is_none());
        assert_eq!(detector.stats().break_reasons.prefix_drifted, 0);
        prefix_test_cleanup(temp_root);
    }

    /// P0-1:相同前缀重复请求 → 无漂移告警(messages 增长被忽略)。
    #[test]
    fn note_request_stable_prefix_no_drift() {
        let (_guard, temp_root) = prefix_test_env("stable");
        let detector = CacheBreakDetector::new("unit-test-prefix");
        let first = request_with_split_system("STATIC", "mem-v1", "turn 1");
        let second = request_with_split_system("STATIC", "mem-v2", "turn 2");
        assert!(detector.note_request(&first).is_none());
        // 动态段(mem)变化 + messages 变化都不应触发请求侧漂移
        assert!(detector.note_request(&second).is_none());
        assert_eq!(detector.stats().break_reasons.prefix_drifted, 0);
        prefix_test_cleanup(temp_root);
    }

    /// P0-1:静态 system 前缀被污染(动态值泄漏进静态区)→ 首 turn 即告警并计数。
    #[test]
    fn note_request_flags_static_prefix_drift() {
        let (_guard, temp_root) = prefix_test_env("static");
        let detector = CacheBreakDetector::new("unit-test-prefix");
        let stable = request_with_split_system("STATIC", "mem", "turn 1");
        let polluted = request_with_split_system("STATIC-2026-09-02", "mem", "turn 2");
        assert!(detector.note_request(&stable).is_none());
        let reason = detector
            .note_request(&polluted)
            .expect("static prefix drift must be flagged");
        assert!(
            reason.contains("system prompt changed"),
            "reason: {reason}"
        );
        assert_eq!(detector.stats().break_reasons.prefix_drifted, 1);
        assert_eq!(detector.stats().break_reasons.system_prompt_changed, 0);
        prefix_test_cleanup(temp_root);
    }

    /// P0-1:工具定义中途变化 → 请求侧告警归因 "tool definitions changed"。
    #[test]
    fn note_request_flags_tool_definition_drift() {
        let (_guard, temp_root) = prefix_test_env("tool");
        let detector = CacheBreakDetector::new("unit-test-prefix");
        let mut request1 = request_with_split_system("STATIC", "mem", "turn 1");
        request1.tools = Some(vec![crate::types::ToolDefinition {
            name: "read_file".to_string(),
            description: Some("v1".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,
        }]);
        let mut request2 = request1.clone();
        request2.tools.as_mut().expect("tools").push(crate::types::ToolDefinition {
            name: "write_file".to_string(),
            description: Some("v2".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,
        });
        assert!(detector.note_request(&request1).is_none());
        let reason = detector
            .note_request(&request2)
            .expect("tool drift must be flagged");
        assert!(
            reason.contains("tool definitions changed"),
            "reason: {reason}"
        );
        assert_eq!(detector.stats().break_reasons.prefix_drifted, 1);
        prefix_test_cleanup(temp_root);
    }
}
