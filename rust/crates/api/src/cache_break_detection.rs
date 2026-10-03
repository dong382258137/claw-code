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

use crate::types::{InputContentBlock, MessageRequest, SystemContent, Usage};

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

// ===========================================================================
// 请求分歧探针(Request Divergence Probe)
// ===========================================================================
// 目的:在 wire 层(`MessageRequest`,即发给上游的真实请求体)记录消息级/块级
// 指纹,做两件事:
//   1. 定位"首个分叉单元"——上一请求与本请求在 [system 块 / tools / 第 N 条
//      消息] 中哪里开始不一致,把整段前缀失效归因到具体单元(而非整体 hash)。
//   2. 轮边界复现——当新一"轮"的首个请求出现(判定见 [`ProbeNote`])时,把
//      「上一请求 + 本请求」的**完整请求体**落盘到
//      `<session_dir>/dumps/req-<seq>.json`,供离线做字节级 diff,复现
//      "轮边界缓存整段失效"(实测:轮首请求 read 回落到仅系统前缀)。
//
// 与 [`CacheBreakDetector`] 的关系:detector 只做整指纹命中率下降检测+分类
// (`claw doctor --cache-stats`);probe 补充"分叉位置 + 请求体证据"。二者
// 独立持久化,互不修改对方状态文件。
//
// 存储(沿用 detector 的 session 目录,见 [`CacheBreakPaths`]):
//   <session_dir>/probe.json            统计(requests/turn_boundaries/dumps)
//   <session_dir>/dumps/req-<seq>.json  轮边界/深层分叉时成对请求体快照
//
// 轮边界判定(不依赖 `is_post_tool`,彼被 runtime 尾部注入段干扰):
// 比较相邻两个请求的消息单元序列,首个分叉点 d 上——
//   - 本请求在该位置被 `role="tool"`(ToolResult)消息顶替 → 轮内工具循环
//     自然增长(尾部位移损耗,已量化 ~2.3k/iter),不落盘;
//   - 被 `user`/`assistant`/其他角色顶替,或被 system/tools 分叉、历史缩短
//     (MessagesShrunk) → 视为轮边界/深层分叉,成对落盘。
// (分类逻辑见 ProbeNote::describe / classify_units)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeMeta {
    pub request_kind: String,
    pub triage_tier: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeStats {
    #[serde(default)]
    pub requests_observed: u64,
    #[serde(default)]
    pub turn_boundaries: u64,
    #[serde(default)]
    pub mid_divergences: u64,
    #[serde(default)]
    pub dumps_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// 落盘触发原因:`turn_boundary` / `system_changed` / `tools_changed` /
    /// `message_divergence` / `messages_shrunk`。`None` 表示仅观察(轮内增长)。
    pub reason: String,
    /// 首个分叉单元描述,如 `sys#2` / `tools` / `msg#5(user)`。
    pub first_divergence: String,
    /// 本次落盘序号(0 = 未落盘)。
    pub dump_seq: u32,
}

/// 单元级指纹:一个可序列化的请求构成块([system 块 | tools | 单条消息])。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProbeUnit {
    tag: String,
    role: String,
    /// 消息内容块类型签名(`text|thinking|tool_use|tool_result`,以 `|` 连接)。
    /// wire 层 tool_result 会被合并进 `role="user"` 消息(见 cli convert_messages),
    /// 不能按 role 判别轮内/轮边界,需按块类型。
    blocks: String,
    len: usize,
    hash: u64,
}

/// 单次请求的单元指纹序列(`units` 顺序 = system 块 → tools → messages)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WireProfile {
    observed_at_unix_ms: u64,
    meta: ProbeMeta,
    units: Vec<ProbeUnit>,
}

#[derive(Debug, Clone)]
struct ProbeInner {
    paths: CacheBreakPaths,
    dumps_dir: PathBuf,
    stats: ProbeStats,
    previous: Option<WireProfile>,
    /// 上一请求的完整请求体(轮边界成对落盘用)。
    prev_request: Option<MessageRequest>,
    seq: u32,
}

/// 轮边界 / 深层分叉时的成对请求体快照。
#[derive(Debug, Serialize)]
struct DumpBody<'a> {
    dump_seq: u32,
    observed_at_unix_ms: u64,
    reason: &'a str,
    first_divergence: String,
    meta_previous: &'a ProbeMeta,
    previous_request: &'a MessageRequest,
    meta_current: &'a ProbeMeta,
    current_request: &'a MessageRequest,
}

/// 磁盘持久化的配对状态:CLI/TUI 每轮 turn 会重建 AnthropicRuntimeClient
/// (见 cli app.rs prepare_turn_runtime → replace_runtime 生命周期),探针内存态
/// (previous / prev_request)随之丢失,跨轮配对断裂 → 轮边界永远捕获不到。
/// 这里把上一请求的指纹+请求体落盘,client 重建后从磁盘恢复配对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedProbeState {
    #[serde(default)]
    seq: u32,
    #[serde(default)]
    previous: Option<WireProfile>,
}

#[derive(Debug, Clone)]
pub struct RequestDivergenceProbe {
    inner: Arc<Mutex<ProbeInner>>,
}

impl RequestDivergenceProbe {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        let paths = CacheBreakPaths::for_session(&session_id.into());
        let dumps_dir = paths.session_dir.join("dumps");
        let stats =
            read_json::<ProbeStats>(&paths.session_dir.join("probe.json")).unwrap_or_default();
        // 从磁盘恢复跨轮配对状态(client 每轮重建,内存态会丢)。
        let persisted: Option<PersistedProbeState> =
            read_json(&paths.session_dir.join("probe-state.json"));
        let previous = persisted.as_ref().and_then(|s| s.previous.clone());
        let prev_request = read_json(&paths.session_dir.join("last-request.json"));
        let seq = std::cmp::max(
            persisted.as_ref().map(|s| s.seq).unwrap_or(0),
            existing_dump_seq(&dumps_dir),
        );
        Self {
            inner: Arc::new(Mutex::new(ProbeInner {
                paths,
                dumps_dir,
                stats,
                previous,
                prev_request,
                seq,
            })),
        }
    }

    #[must_use]
    pub fn stats(&self) -> ProbeStats {
        self.lock().stats.clone()
    }

    /// 探针持久化路径(session 目录;body dump 在 `<session_dir>/dumps` 下)。
    #[must_use]
    pub fn paths(&self) -> CacheBreakPaths {
        self.lock().paths.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ProbeInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 请求发出前调用。返回 `Some(report)` 表示捕获到轮边界/深层分叉(已落盘
    /// 成对请求体或仅记录);`None` 表示轮内自然增长,无需关注。
    #[must_use]
    pub fn note_request(&self, request: &MessageRequest, meta: ProbeMeta) -> Option<ProbeReport> {
        if probe_disabled() || meta.request_kind != "Main" {
            return None;
        }
        let profile = WireProfile {
            observed_at_unix_ms: now_unix_ms(),
            meta: meta.clone(),
            units: profile_units(request),
        };

        let mut inner = self.lock();
        inner.stats.requests_observed += 1;

        // 先只读分类(避免对 inner 的可变更新与不可变借用冲突),事件数据取快照。
        let classified = inner.previous.as_ref().map(|prev| {
            (
                classify_units(prev.units.as_slice(), &profile.units),
                prev.meta.clone(),
            )
        });

        let report = match classified {
            None => None, // 会话首请求:仅建基线
            Some((note, prev_meta)) => {
                let (reason, first_divergence) = note.describe();
                if let Some(reason) = reason {
                    if let Some(prev_request) = inner.prev_request.clone() {
                        inner.seq += 1;
                        let dump_seq = inner.seq;
                        if persist_dump(
                            &inner.dumps_dir,
                            &DumpBody {
                                dump_seq,
                                observed_at_unix_ms: profile.observed_at_unix_ms,
                                reason: &reason,
                                first_divergence: first_divergence.clone(),
                                meta_previous: &prev_meta,
                                previous_request: &prev_request,
                                meta_current: &meta,
                                current_request: request,
                            },
                        ) {
                            inner.stats.dumps_written += 1;
                        }
                        if note.is_turn_boundary() {
                            inner.stats.turn_boundaries += 1;
                        } else {
                            inner.stats.mid_divergences += 1;
                        }
                        Some(ProbeReport {
                            reason,
                            first_divergence,
                            dump_seq,
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        inner.previous = Some(profile);
        inner.prev_request = Some(request.clone());
        persist_probe(&inner);
        persist_pairing_state(&inner, request);
        report
    }
}

/// 落盘配对状态:指纹 + 上一请求体 + 序号。client 每轮重建后从磁盘恢复,
/// 保证跨轮(乃至跨进程)的轮边界成对捕获不丢失。
fn persist_pairing_state(inner: &ProbeInner, request: &MessageRequest) {
    let state = PersistedProbeState {
        seq: inner.seq,
        previous: inner.previous.clone(),
    };
    let _ = fs::create_dir_all(&inner.paths.session_dir);
    let _ = write_json(&inner.paths.session_dir.join("probe-state.json"), &state);
    // 上一请求体(为下一请求的成对落盘准备)。单文件覆盖,不增长。
    let _ = write_json(&inner.paths.session_dir.join("last-request.json"), request);
}

/// 分歧分类:比较相邻两个请求的单元序列,确定"首个分叉点"属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeNote {
    /// 上一请求是当前请求的严格前缀(轮内纯增长,尾部无 shift)。
    TailGrowth,
    /// 分叉点落在尾部注入段 zone(最后 1-2 单元被 ToolResult 顶替)。
    TailShift,
    /// system 第 i 个块分叉。
    SystemBlock(usize),
    /// tools 整集分叉(含分诊裁剪)。
    ToolsChanged,
    /// 第 i 条消息深层分叉(不在尾部位移 zone)。
    MessageMid(usize),
    /// 轮边界:尾部位移 zone 被非 tool 消息(新 user 输入/新 assistant)顶替。
    TurnBoundary(usize),
    /// 当前请求消息数少于上一请求(历史被压缩/截断)。
    MessagesShrunk,
}

impl ProbeNote {
    /// 产出 (reason 标签, 首分叉描述)。`None` = 轮内自然增长(不落盘)。
    /// 轮内工具循环(`role="tool"` 顶替)在 classify 阶段已归 TailShift。
    fn describe(self) -> (Option<String>, String) {
        match self {
            ProbeNote::TailGrowth | ProbeNote::TailShift => (None, String::new()),
            ProbeNote::SystemBlock(i) => (Some("system_changed".into()), format!("sys#{i}")),
            ProbeNote::ToolsChanged => (Some("tools_changed".into()), "tools".into()),
            ProbeNote::MessageMid(i) => (Some("message_divergence".into()), format!("msg#{i}")),
            ProbeNote::TurnBoundary(i) => (Some("turn_boundary".into()), format!("msg#{i}")),
            ProbeNote::MessagesShrunk => (Some("messages_shrunk".into()), "len-short".into()),
        }
    }

    /// 是否视为"轮边界"(新用户消息后的首个请求)—— 用于统计 turn_boundaries。
    fn is_turn_boundary(self) -> bool {
        matches!(self, Self::TurnBoundary(_))
    }
}

/// 计算首个分叉单元索引(上一请求 vs 当前请求,按顺序单元)。无分叉返回 None。
fn first_divergence_index(prev_units: &[ProbeUnit], curr_units: &[ProbeUnit]) -> Option<usize> {
    prev_units
        .iter()
        .zip(curr_units.iter())
        .position(|(previous, current)| previous != current)
}

fn classify_units(prev_units: &[ProbeUnit], curr_units: &[ProbeUnit]) -> ProbeNote {
    match first_divergence_index(prev_units, curr_units) {
        None => {
            if curr_units.len() >= prev_units.len() {
                ProbeNote::TailGrowth
            } else {
                ProbeNote::MessagesShrunk
            }
        }
        Some(d) => {
            let tag = curr_units
                .get(d)
                .map(|u| u.tag.as_str())
                .or_else(|| prev_units.get(d).map(|u| u.tag.as_str()))
                .unwrap_or("");
            if tag == "tools" {
                ProbeNote::ToolsChanged
            } else if let Some(rest) = tag.strip_prefix("sys#") {
                let idx = rest.split('^').next().unwrap_or("0");
                ProbeNote::SystemBlock(idx.parse().unwrap_or(0))
            } else if let Some(rest) = tag.strip_prefix("msg#") {
                let msg_idx = rest.split('^').next().unwrap_or("0").parse().unwrap_or(d);
                // 尾部位移 zone:分叉点落在上一请求最后 1-2 个单元(注入段 S/H)。
                let tail_zone = d >= prev_units.len().saturating_sub(2) && prev_units.len() >= 2;
                if tail_zone {
                    // 轮内工具循环:该位置被「assistant tool_use 或合并的
                    // tool_result」顶替(实测:wire 层 tool_use 在 tool_result
                    // 之前,首迭代时占据尾部落差位置);
                    // 轮边界:被纯文本(新用户输入 / 上一轮最终 assistant text)
                    // 顶替。
                    let blocks = curr_units.get(d).map(|u| u.blocks.as_str()).unwrap_or("");
                    let is_tool_related =
                        blocks.contains("tool_result") || blocks.contains("tool_use");
                    if is_tool_related {
                        ProbeNote::TailShift
                    } else {
                        ProbeNote::TurnBoundary(msg_idx)
                    }
                } else {
                    ProbeNote::MessageMid(msg_idx)
                }
            } else {
                ProbeNote::MessagesShrunk
            }
        }
    }
}

/// 构建请求的单元指纹序列:system 块 → tools → 逐条消息。
fn profile_units(request: &MessageRequest) -> Vec<ProbeUnit> {
    let mut units = Vec::new();
    if let Some(system) = &request.system {
        match system {
            SystemContent::Text(text) => {
                units.push(ProbeUnit {
                    tag: "sys#0".into(),
                    role: "system".into(),
                    blocks: String::new(),
                    len: text.len(),
                    hash: hash_string(text),
                });
            }
            SystemContent::Blocks(blocks) => {
                for (i, block) in blocks.iter().enumerate() {
                    let dynamic = block.cache_control.is_none();
                    let bytes = serde_json::to_vec(block).unwrap_or_default();
                    units.push(ProbeUnit {
                        tag: format!("sys#{i}{}", if dynamic { "^dyn" } else { "" }),
                        role: "system".into(),
                        blocks: String::new(),
                        len: bytes.len(),
                        hash: stable_hash_bytes(&bytes),
                    });
                }
            }
        }
    }
    if let Some(tools) = &request.tools {
        let bytes = serde_json::to_vec(tools).unwrap_or_default();
        units.push(ProbeUnit {
            tag: "tools".into(),
            role: "tools".into(),
            blocks: String::new(),
            len: bytes.len(),
            hash: stable_hash_bytes(&bytes),
        });
    }
    for (i, message) in request.messages.iter().enumerate() {
        let kinds: Vec<&str> = message
            .content
            .iter()
            .map(|block| match block {
                InputContentBlock::Text { .. } => "text",
                InputContentBlock::Thinking { .. } => "thinking",
                InputContentBlock::ToolUse { .. } => "tool_use",
                InputContentBlock::ToolResult { .. } => "tool_result",
            })
            .collect();
        let bytes = serde_json::to_vec(message).unwrap_or_default();
        units.push(ProbeUnit {
            tag: format!("msg#{i}"),
            role: message.role.clone(),
            blocks: kinds.join("|"),
            len: bytes.len(),
            hash: stable_hash_bytes(&bytes),
        });
    }
    units
}

fn persist_probe(inner: &ProbeInner) {
    let _ = fs::create_dir_all(&inner.paths.session_dir);
    let _ = write_json(&inner.paths.session_dir.join("probe.json"), &inner.stats);
}

fn persist_dump(dumps_dir: &Path, body: &DumpBody<'_>) -> bool {
    if fs::create_dir_all(dumps_dir).is_err() {
        return false;
    }
    let path = dumps_dir.join(format!("req-{:05}.json", body.dump_seq));
    match serde_json::to_vec_pretty(body) {
        Ok(bytes) => fs::write(path, bytes).is_ok(),
        Err(_) => false,
    }
}

/// 扫描已有 dump 序号,新进程续写不覆盖。跨进程序号可能复用,但 session_id
/// 基本唯一,且文件名带时间戳信息(见 manifest),风险可接受。
fn existing_dump_seq(dumps_dir: &Path) -> u32 {
    let Ok(entries) = fs::read_dir(dumps_dir) else {
        return 0;
    };
    let mut max = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(rest) = name.strip_prefix("req-") {
            if let Some(num) = rest.strip_suffix(".json") {
                if let Ok(n) = num.parse::<u32>() {
                    max = max.max(n);
                }
            }
        }
    }
    max
}

/// 环境变量 `RUSTY_CLAUDE_PROBE_DISABLE=1` 可完全关闭探针(无磁盘写)。
fn probe_disabled() -> bool {
    std::env::var("RUSTY_CLAUDE_PROBE_DISABLE")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
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
        classify_units, detect_cache_break, read_json, request_hash_hex, sanitize_path_segment,
        CacheBreakConfig, CacheBreakDetector, CacheBreakPaths, ProbeMeta, ProbeNote, ProbeStats,
        ProbeUnit, RequestDivergenceProbe, TrackedPromptState,
    };
    use crate::types::{
        CacheControl, InputContentBlock, InputMessage, MessageRequest, SystemBlock, SystemContent,
        Usage,
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

    #[allow(dead_code)]
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
    fn prefix_test_env(
        guard_tag: &str,
    ) -> (std::sync::MutexGuard<'static, ()>, std::path::PathBuf) {
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
        assert!(reason.contains("system prompt changed"), "reason: {reason}");
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
        request2
            .tools
            .as_mut()
            .expect("tools")
            .push(crate::types::ToolDefinition {
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

    // ---------------- RequestDivergenceProbe tests ----------------

    fn probe_unit(tag: &str, role: &str, blocks: &str) -> ProbeUnit {
        ProbeUnit {
            tag: tag.to_string(),
            role: role.to_string(),
            blocks: blocks.to_string(),
            len: 1,
            hash: 0,
        }
    }

    /// 轮内纯增长(prev 是 curr 严格前缀)→ TailGrowth(不落盘)。
    #[test]
    fn probe_classifies_tail_growth() {
        let prev = vec![probe_unit("msg#0", "user", "text")];
        let curr = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
        ];
        assert_eq!(classify_units(&prev, &curr), ProbeNote::TailGrowth);
    }

    /// 轮内工具循环:尾部位移 zone 被 tool_result 块顶替 → TailShift(不落盘,
    /// 即已量化的 ~2.3k/iter 结构性损耗,不是本轮要抓的异常)。
    #[test]
    fn probe_classifies_in_turn_tool_loop_as_tail_shift() {
        // prev = [u0, a1(tool_use), S(user text), H(user text)]
        let prev = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
            probe_unit("msg#5", "user", "text"),
            probe_unit("msg#6", "user", "text"),
        ];
        // curr = [u0, a1(相同), TR(合并进 user,tool_result 块), S, H]
        let curr = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
            probe_unit("msg#2", "user", "tool_result"),
            probe_unit("msg#5", "user", "text"),
            probe_unit("msg#6", "user", "text"),
        ];
        assert_eq!(classify_units(&prev, &curr), ProbeNote::TailShift);
    }

    /// 2026-09-19 实测抓包回归:turn1 初始请求 [fm, u0, S, H] → turn1 工具
    /// 续写请求 [fm, u0, a(tool_use), u(tool_result), S, H]。wire 层
    /// assistant tool_use 占据尾部落差位置(tool_result 在其后合并进 user),
    /// 此前的判别只认 tool_result 导致误判为轮边界。修复后必须归 TailShift。
    #[test]
    fn probe_classifies_tool_use_at_tail_zone_as_tail_shift() {
        // prev = turn1 初始请求: [固定记忆(fm), u0, S(Project context), H(Runtime context)]
        let prev = vec![
            probe_unit("msg#0", "user", "text"), // 固定记忆
            probe_unit("msg#1", "user", "text"), // 用户 round-1 输入
            probe_unit("msg#2", "user", "text"), // # Project context 尾部注入
            probe_unit("msg#3", "user", "text"), // # Runtime Context 尾部注入
        ];
        // curr = turn1 工具续写: 尾部注入段位移,assistant tool_use 顶到 msg#2
        let curr = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "user", "text"),
            probe_unit("msg#2", "assistant", "tool_use"), // 新工具调用
            probe_unit("msg#3", "user", "tool_result"),   // 合并的 tool_result
            probe_unit("msg#4", "user", "text"),          // S(Project context)
            probe_unit("msg#5", "user", "text"),          // H(Runtime context)
        ];
        assert_eq!(classify_units(&prev, &curr), ProbeNote::TailShift);
    }

    /// 真实轮边界:上一轮末请求 [..., S, H] → 新一轮首请求 [..., 新 user 输入(text), S, H]。
    #[test]
    fn probe_classifies_turn_boundary_with_new_user_text() {
        let prev = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
            probe_unit("msg#2", "user", "tool_result"),
            probe_unit("msg#5", "user", "text"), // S
            probe_unit("msg#6", "user", "text"), // H
        ];
        let curr = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
            probe_unit("msg#2", "user", "tool_result"),
            probe_unit("msg#3", "user", "text"), // 新一轮用户输入
            probe_unit("msg#5", "user", "text"), // S
            probe_unit("msg#6", "user", "text"), // H
        ];
        assert_eq!(classify_units(&prev, &curr), ProbeNote::TurnBoundary(3));
    }

    /// 真实轮边界变体:上一轮末请求 [..., S, H] → 新一轮首请求 含上一轮最终
    /// assistant 文本块 [..., a(final text), u_new, S, H],分叉单元为 assistant 文本。
    #[test]
    fn probe_classifies_turn_boundary_with_assistant_final_text() {
        let prev = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
            probe_unit("msg#2", "user", "tool_result"),
            probe_unit("msg#5", "user", "text"), // S
            probe_unit("msg#6", "user", "text"), // H
        ];
        let curr = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
            probe_unit("msg#2", "user", "tool_result"),
            probe_unit("msg#3", "assistant", "text"), // 上一轮最终答复
            probe_unit("msg#4", "user", "text"),      // 新一轮用户输入
            probe_unit("msg#5", "user", "text"),      // S
            probe_unit("msg#6", "user", "text"),      // H
        ];
        assert_eq!(classify_units(&prev, &curr), ProbeNote::TurnBoundary(3));
    }

    /// 轮边界:尾部位移 zone 被新的纯文本 user 消息顶替 → TurnBoundary(落盘)。
    #[test]
    fn probe_classifies_turn_boundary() {
        // prev = 上一轮最后一个请求:[..., TR, S, H]
        let prev = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
            probe_unit("msg#2", "user", "tool_result"),
            probe_unit("msg#5", "user", "text"),
            probe_unit("msg#6", "user", "text"),
        ];
        // curr = 新一轮首请求:[..., TR, 新 user 输入(text), S, H]
        let curr = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "tool_use"),
            probe_unit("msg#2", "user", "tool_result"),
            probe_unit("msg#3", "user", "text"),
            probe_unit("msg#5", "user", "text"),
            probe_unit("msg#6", "user", "text"),
        ];
        assert_eq!(classify_units(&prev, &curr), ProbeNote::TurnBoundary(3));
    }

    /// 深层分叉(消息区中部内容变化,如 fixed_memory 重写注入 messages[0])
    /// → MessageMid(落盘)。这正是轮边界整段失效的头号嫌疑。
    #[test]
    fn probe_classifies_deep_message_divergence() {
        // prev 4 条消息,第 1 条消息内容变化(仅 hash 不同)→ d=1,不在尾部位移
        // zone(prev.len()-2=2)→ MessageMid(1)。
        let prev = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "user", "text"),
            probe_unit("msg#2", "user", "text"),
            probe_unit("msg#3", "user", "text"),
        ];
        let mut curr = prev.clone();
        curr[1].hash = 42; // 第 1 条消息内容变化
        assert_eq!(classify_units(&prev, &curr), ProbeNote::MessageMid(1));
    }

    /// system 块分叉 → SystemBlock(落盘)。
    #[test]
    fn probe_classifies_system_block_change() {
        let prev = vec![probe_unit("sys#0", "system", "")];
        let curr = vec![probe_unit("sys#0", "system", "")];
        // 需要用不同 hash 表示内容变化
        let mut prev = prev;
        let mut curr = curr;
        prev[0].hash = 1;
        curr[0].hash = 2;
        assert_eq!(classify_units(&prev, &curr), ProbeNote::SystemBlock(0));
    }

    /// 历史缩短(压缩/截断)→ MessagesShrunk(落盘)。
    #[test]
    fn probe_classifies_messages_shrunk() {
        let prev = vec![
            probe_unit("msg#0", "user", "text"),
            probe_unit("msg#1", "assistant", "text"),
        ];
        let curr = vec![probe_unit("msg#0", "user", "text")];
        assert_eq!(classify_units(&prev, &curr), ProbeNote::MessagesShrunk);
    }

    /// 端到端:轮边界成对请求体落盘 + 轮内增长不落盘。
    #[test]
    fn probe_dumps_pair_on_turn_boundary_only() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "probe-dump-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        let probe = RequestDivergenceProbe::new("unit-test-probe");
        let meta = ProbeMeta {
            request_kind: "Main".to_string(),
            triage_tier: Some("Standard".to_string()),
        };

        // 1) 会话首请求:仅建基线,不落盘。
        let first = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            messages: vec![
                InputMessage::user_text("hello"),
                InputMessage {
                    role: "assistant".into(),
                    content: vec![InputContentBlock::ToolUse {
                        id: "call_01_x".into(),
                        name: "noop".into(),
                        input: serde_json::json!({}),
                    }],
                },
                InputMessage::user_text("snapshot"),
                InputMessage::user_text("hints"),
            ],
            ..Default::default()
        };
        assert!(probe.note_request(&first, meta.clone()).is_none());

        // 2) 轮内工具循环(带 tool_result 的续写请求)→ 不落盘。
        let in_turn = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            messages: vec![
                InputMessage::user_text("hello"),
                InputMessage {
                    role: "assistant".into(),
                    content: vec![InputContentBlock::ToolUse {
                        id: "call_01_x".into(),
                        name: "noop".into(),
                        input: serde_json::json!({}),
                    }],
                },
                InputMessage {
                    role: "user".into(),
                    content: vec![InputContentBlock::ToolResult {
                        tool_use_id: "call_01_x".into(),
                        content: vec![crate::types::ToolResultContentBlock::Text {
                            text: "ok".into(),
                        }],
                        is_error: false,
                    }],
                },
                InputMessage::user_text("snapshot"),
                InputMessage::user_text("hints"),
            ],
            ..Default::default()
        };
        assert!(probe.note_request(&in_turn, meta.clone()).is_none());

        // 3) 轮边界(新用户消息)→ 落盘成对请求体,report.reason == "turn_boundary"。
        let turn_first = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            messages: vec![
                InputMessage::user_text("hello"),
                InputMessage {
                    role: "assistant".into(),
                    content: vec![InputContentBlock::ToolUse {
                        id: "call_01_x".into(),
                        name: "noop".into(),
                        input: serde_json::json!({}),
                    }],
                },
                InputMessage {
                    role: "user".into(),
                    content: vec![InputContentBlock::ToolResult {
                        tool_use_id: "call_01_x".into(),
                        content: vec![crate::types::ToolResultContentBlock::Text {
                            text: "ok".into(),
                        }],
                        is_error: false,
                    }],
                },
                InputMessage::user_text("新的一轮开始"),
                InputMessage::user_text("snapshot"),
                InputMessage::user_text("hints"),
            ],
            ..Default::default()
        };
        let report = probe
            .note_request(&turn_first, meta.clone())
            .expect("turn boundary should be captured");
        assert_eq!(report.reason, "turn_boundary");
        assert_eq!(report.dump_seq, 1);

        // dump 文件存在且含 prev + current 两个请求体。
        let dumps_dir = probe.paths().session_dir.join("dumps");
        let dump_path = dumps_dir.join("req-00001.json");
        assert!(dump_path.exists(), "dump file should exist: {dump_path:?}");
        let raw = std::fs::read_to_string(&dump_path).expect("read dump");
        assert!(
            raw.contains("previous_request"),
            "dump should include prev body"
        );
        assert!(
            raw.contains("current_request"),
            "dump should include curr body"
        );
        // 解析 JSON 校验 current_request 携带新轮输入(避免字符串转义歧义)。
        let dump: serde_json::Value = serde_json::from_str(&raw).expect("dump is valid json");
        let curr_msgs = dump["current_request"]["messages"]
            .as_array()
            .expect("current_request.messages");
        assert_eq!(
            curr_msgs.len(),
            6,
            "current request should carry 6 messages"
        );
        assert_eq!(
            curr_msgs[3]["content"][0]["text"].as_str(),
            Some("新的一轮开始"),
            "current request should carry the new turn input"
        );

        // probe.json 统计落盘。
        let stats_path = probe.paths().session_dir.join("probe.json");
        let persisted = read_json::<ProbeStats>(&stats_path).expect("probe.json");
        assert_eq!(persisted.requests_observed, 3);
        assert_eq!(persisted.turn_boundaries, 1);
        assert_eq!(persisted.dumps_written, 1);

        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }

    /// 跨轮配对恢复:CLI 每轮 turn 重建 AnthropicRuntimeClient(内存配对态丢失),
    /// 新 probe 实例应能从磁盘恢复上一请求,使轮边界仍可成对落盘。
    #[test]
    fn probe_restores_pairing_across_client_rebuild() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "probe-rebuild-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        let meta = ProbeMeta {
            request_kind: "Main".to_string(),
            triage_tier: Some("Standard".to_string()),
        };
        // turn1 末请求 [fm, u0, a1(tool_use), u2(tool_result), S, H] → 基线观察。
        let turn1_last = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            messages: vec![
                InputMessage::user_text("fm"),
                InputMessage::user_text("hello"),
                InputMessage {
                    role: "assistant".into(),
                    content: vec![InputContentBlock::ToolUse {
                        id: "call_01_x".into(),
                        name: "noop".into(),
                        input: serde_json::json!({}),
                    }],
                },
                InputMessage {
                    role: "user".into(),
                    content: vec![InputContentBlock::ToolResult {
                        tool_use_id: "call_01_x".into(),
                        content: vec![crate::types::ToolResultContentBlock::Text {
                            text: "ok".into(),
                        }],
                        is_error: false,
                    }],
                },
                InputMessage::user_text("S"),
                InputMessage::user_text("H"),
            ],
            ..Default::default()
        };
        let p1 = RequestDivergenceProbe::new("rebuild-test");
        assert!(p1.note_request(&turn1_last, meta.clone()).is_none());

        // 模拟 client 每轮重建:new 一个新实例(内存 previous=None,从磁盘恢复)。
        let p2 = RequestDivergenceProbe::new("rebuild-test");
        let turn2_first = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            messages: vec![
                InputMessage::user_text("fm"),
                InputMessage::user_text("hello"),
                InputMessage {
                    role: "assistant".into(),
                    content: vec![InputContentBlock::ToolUse {
                        id: "call_01_x".into(),
                        name: "noop".into(),
                        input: serde_json::json!({}),
                    }],
                },
                InputMessage {
                    role: "user".into(),
                    content: vec![InputContentBlock::ToolResult {
                        tool_use_id: "call_01_x".into(),
                        content: vec![crate::types::ToolResultContentBlock::Text {
                            text: "ok".into(),
                        }],
                        is_error: false,
                    }],
                },
                InputMessage::user_text("新的一轮"), // 新用户输入
                InputMessage::user_text("S"),
                InputMessage::user_text("H"),
            ],
            ..Default::default()
        };
        let report = p2
            .note_request(&turn2_first, meta)
            .expect("turn boundary should be captured across client rebuild");
        assert_eq!(report.reason, "turn_boundary");
        let dump = p2.paths().session_dir.join("dumps/req-00001.json");
        assert!(dump.exists(), "pair dump should exist: {dump:?}");
        let dump_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&dump).expect("read dump"))
                .expect("valid json");
        assert_eq!(
            dump_json["current_request"]["messages"]
                .as_array()
                .expect("messages")[4]["content"][0]["text"],
            "新的一轮"
        );

        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }

    /// `RUSTY_CLAUDE_PROBE_DISABLE=1` → 完全关闭(不写盘)。
    #[test]
    fn probe_can_be_disabled_via_env() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "probe-disable-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        std::env::set_var("RUSTY_CLAUDE_PROBE_DISABLE", "1");
        let probe = RequestDivergenceProbe::new("unit-test-probe-off");
        let meta = ProbeMeta {
            request_kind: "Main".to_string(),
            triage_tier: None,
        };
        let first = MessageRequest {
            model: "deepseek-v4-flash".to_string(),
            messages: vec![InputMessage::user_text("hi")],
            ..Default::default()
        };
        assert!(probe.note_request(&first, meta.clone()).is_none());
        assert!(probe.note_request(&first, meta).is_none());
        std::env::remove_var("RUSTY_CLAUDE_PROBE_DISABLE");
        let _ = std::fs::remove_dir_all(temp_root); // 禁用时无文件生成,目录可能不存在
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }
}
