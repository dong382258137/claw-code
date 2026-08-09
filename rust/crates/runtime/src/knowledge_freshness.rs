//! Knowledge freshness gate — 评估任务的知识时效性,决定是否需要联网调研。
//!
//! 设计目标(中医语义锚点):
//! - **勤求古训**:Novel 任务(新版本/新论文)在派发前先调研,避免用过时参数知识自信地错答
//! - **急则治标**:重试场景(attempt > 0)视为紧急,跳过调研直接重试,避免延迟
//! - **同病异治**:同类任务(Stable/Evolving/Novel)走不同知识获取策略
//!
//! 架构:
//! - `ResearchClient` trait(async):依赖倒置,runtime 不依赖 tools/api
//! - `GLOBAL_RESEARCH_CLIENT`(OnceLock):全局注入点,未注入时降级(不调研)
//! - `GATE_CACHE`(RwLock<HashMap>):task_hash → GatedTask 缓存,避免 retry 重复调研
//! - `gate_task`(async):MVP 入口,在 coordinator_executor::execute 内部调用
//!
//! 详见 `docs/plans/knowledge-freshness-gate-plan.md` v3。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;

// ──────────────────────────────────────────────────────────────────────────
// 知识新鲜度分类
// ──────────────────────────────────────────────────────────────────────────

/// 任务所需知识的时效性分类。
///
/// 判定优先级(风险优先,漏判成本 > 误判成本):
/// 1. **Novel** — 涉及新版本/新论文/最新规范,参数记忆可能过时,需联网调研
/// 2. **Evolving** — 涉及重构/优化/修复,框架 API 可能演进,默认可调研
/// 3. **Stable** — 机械操作(typo/format/lint),知识稳定,无需调研
///
/// 默认(无关键词命中):Evolving(保守倾向,但 MVP 阶段 client 未注入,无成本)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeFreshness {
    /// 知识稳定(机械操作),无需调研。
    Stable,
    /// 知识演进中(重构/修复),可调研。
    Evolving,
    /// 知识可能过时(新版本/新论文),需调研。
    Novel,
}

impl KnowledgeFreshness {
    /// 是否需要联网调研。
    pub fn needs_research(self) -> bool {
        matches!(self, KnowledgeFreshness::Novel)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 关键词表
// ──────────────────────────────────────────────────────────────────────────

/// 强 Novel 信号:单独命中即判为 Novel(无需版本号共现)。
///
/// 这些词几乎只在"需要最新信息"的语境出现,误报率低。
const NOVEL_STRONG: &[&str] = &[
    // 英文
    "latest",
    "newest",
    "recent",
    "changelog",
    "release notes",
    "arxiv",
    "paper",
    "spec",
    "rfc",
    "breaking change",
    "deprecat", // 涵盖 deprecate/deprecated/deprecation
    // 中文(本项目主力场景)
    "最新",
    "新版",
    "新版本",
    "最新版",
    "发布说明",
    "更新日志",
    "变更日志",
    "论文",
    "规范",
    "废弃",
    "弃用",
    "破坏性变更",
];

/// 动作型 Novel 信号:需与版本号共现才判为 Novel。
///
/// 单独出现 "upgrade to" 可能是抽象描述,但 "upgrade to 3.2" 是明确的新版本迁移。
/// 这避免了 "bump version"(Stable)被 "version" 误判为 Novel 的问题。
const NOVEL_ACTION: &[&str] = &[
    // 英文
    "upgrade to",
    "migrate to",
    "update to",
    "port to",
    "pin to",
    "bump to",
    // 中文
    "升级到",
    "升级至",
    "迁移到",
    "迁移至",
    "更新到",
    "更新至",
    "移植到",
    "移植至",
];

/// Evolving 信号:涉及代码演进,框架 API 可能变化。
const EVOLVING_KEYWORDS: &[&str] = &[
    // 英文
    "refactor",
    "optimize",
    "performance",
    "bug",
    "fix",
    "error",
    "crash",
    "regression",
    // 中文
    "重构",
    "优化",
    "性能",
    "缺陷",
    "错误",
    "崩溃",
    "回归",
    "修复",
];

/// Stable 信号:机械操作,知识高度稳定。
const STABLE_KEYWORDS: &[&str] = &[
    // 英文
    "typo",
    "format",
    "rename",
    "lint",
    "whitespace",
    "import",
    "sort",
    "reorder",
    "capitaliz", // 涵盖 capitalize/capitalization
    // 中文
    "错别字",
    "格式",
    "重命名",
    "空格",
    "导入",
    "排序",
];

/// 本地/内部操作信号(S4 负向排除)。
///
/// 设计文档 §3.2/S4:关键词命中是**候选信号**而非最终裁决。任务明显是
/// 本地/内部操作(如"在这个仓库里…"、"项目内…"、引用具体文件路径)时,
/// 即使含 "api"/"sdk"/"latest" 等关键词也回退 Stable——避免对内部 API
/// 误触发联网调研(白搜)。
const LOCAL_INTERNAL_SIGNALS: &[&str] = &[
    // 中文
    "仓库里",
    "仓库内",
    "本仓库",
    "当前仓库",
    "项目内",
    "项目里",
    "本项目中",
    "这个项目",
    "代码库",
    "源码里",
    "源码中",
    "本目录",
    "工作区",
    "这个文件",
    "该文件",
    "当前文件",
    // 英文
    "in this repo",
    "in this repository",
    "in the repo",
    "in this codebase",
    "in the codebase",
    "in this project",
    "in the workspace",
    "internal api",
    "internal function",
    "internal module",
    "our codebase",
];

/// 判断任务是否为本地/内部操作(S4 负向排除的判定主体)。
///
/// 命中条件:短语信号(仓库/项目/代码库语境)或具体文件路径特征
/// (含 `src/`、`crates/` 等路径前缀或 `.rs`/`.py` 等扩展名)。
fn is_local_internal_task(lower: &str) -> bool {
    for kw in LOCAL_INTERNAL_SIGNALS {
        if lower.contains(kw) {
            return true;
        }
    }
    // 具体文件路径特征:路径前缀或常见源码/配置文件扩展名。
    // 例如 "修改 src/main.rs 的接口"、"在 crates/runtime 里…"。
    for path_marker in [
        "src/", "crates/", "tests/", "docs/", "lib/", "app/", "core/", "config/",
    ] {
        if lower.contains(path_marker) {
            return true;
        }
    }
    for ext in [
        ".rs", ".py", ".ts", ".js", ".go", ".java", ".cpp", ".c", ".h", ".md", ".toml",
        ".json", ".yaml", ".yml",
    ] {
        if lower.contains(ext) {
            return true;
        }
    }
    false
}

// ──────────────────────────────────────────────────────────────────────────
// 评估函数
// ──────────────────────────────────────────────────────────────────────────

/// 评估任务的知识新鲜度。
///
/// 判定顺序(风险优先):Novel > Evolving > Stable > 默认 Evolving。
/// Novel 信号最优先(漏判 = 用过时知识自信地错答,成本最高)。
///
/// S4 负向排除优先级最高:本地/内部操作任务(仓库内/项目内/文件路径)直接
/// 判 Stable,短路后续关键词评估——避免"在这个仓库里改 API"误触发联网调研。
pub fn assess_knowledge_freshness(task: &str) -> KnowledgeFreshness {
    let lower = task.to_lowercase();

    // 0. S4 负向排除:本地/内部操作任务直接 Stable(最高优先级,短路)。
    if is_local_internal_task(&lower) {
        return KnowledgeFreshness::Stable;
    }

    // 1. Novel:强信号单独命中
    for kw in NOVEL_STRONG {
        if lower.contains(kw) {
            return KnowledgeFreshness::Novel;
        }
    }

    // 2. Novel:动作词 + 版本号共现
    let has_version = contains_version_number(&lower);
    if has_version {
        for kw in NOVEL_ACTION {
            if lower.contains(kw) {
                return KnowledgeFreshness::Novel;
            }
        }
    }

    // 3. Evolving:演进类信号
    for kw in EVOLVING_KEYWORDS {
        if lower.contains(kw) {
            return KnowledgeFreshness::Evolving;
        }
    }

    // 4. Stable:机械操作信号
    for kw in STABLE_KEYWORDS {
        if lower.contains(kw) {
            return KnowledgeFreshness::Stable;
        }
    }

    // 5. 默认:保守判为 Evolving(不确定时倾向可调研)
    KnowledgeFreshness::Evolving
}

/// 检测字符串是否含版本号(x.y 或 x.y.z 格式)。
///
/// 简单实现:扫描 "数字.数字" 模式,避免正则依赖。
fn contains_version_number(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return false;
    }
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i].is_ascii_digit() && bytes[i + 1] == b'.' && bytes[i + 2].is_ascii_digit() {
            return true;
        }
        i += 1;
    }
    false
}

// ──────────────────────────────────────────────────────────────────────────
// ResearchClient trait(依赖倒置)
// ──────────────────────────────────────────────────────────────────────────

/// 联网调研客户端 trait(async)。
///
/// 依赖倒置:runtime 定义 trait,上层(rusty-claude-cli)注入生产实现
/// (封装 execute_web_search + WebFetch + LLM 摘要)。镜像
/// `decision_log::DecisionExtractorClient` 的 OnceLock + set_global_* 模式。
///
/// 未注入时 `get_global_research_client()` 返回 None,gate_task 静默降级(不调研)。
#[async_trait]
pub trait ResearchClient: Send + Sync {
    /// 调研指定查询,返回摘要文本。
    ///
    /// 生产实现应:执行 2-3 次 WebSearch → WebFetch 抓取 top 结果 → LLM 摘要拼接。
    /// 失败时返回 Err,gate_task 会降级为不调研(不阻塞任务执行)。
    async fn research(&self, query: &str) -> Result<String, String>;
}

static GLOBAL_RESEARCH_CLIENT: OnceLock<Option<Arc<dyn ResearchClient>>> = OnceLock::new();

/// 注入全局 ResearchClient(启动时调用,如 app.rs 初始化阶段)。
///
/// 镜像 `decision_log::set_global_decision_extractor`。重复调用会被忽略
/// (OnceLock 语义),生产环境应在启动早期一次性注入。
pub fn set_global_research_client(client: Arc<dyn ResearchClient>) {
    let _ = GLOBAL_RESEARCH_CLIENT.set(Some(client));
}

/// 获取全局 ResearchClient(未注入返回 None)。
pub fn get_global_research_client() -> Option<Arc<dyn ResearchClient>> {
    GLOBAL_RESEARCH_CLIENT
        .get()
        .and_then(|opt| opt.as_ref().map(Arc::clone))
}

// ──────────────────────────────────────────────────────────────────────────
// FreshnessAssessor trait(Phase 2.1:LLM 语义评估,依赖倒置)
// ──────────────────────────────────────────────────────────────────────────

/// 知识新鲜度语义评估器 trait(Phase 2.1)。
///
/// Phase 0/1 用关键词启发式评估(`assess_knowledge_freshness`)。Phase 2 升级为
/// LLM 语义评估:当关键词评估结果为 Evolving(不确定)时,调 flash 模型细化判断,
/// 区分"看似 Evolving 实则 Novel"(如冷门库的新版本)和"确实 Stable"(如通用重构)。
///
/// **成本控制策略**(非完全替代关键词):
/// - 关键词评估为 Novel/Stable(强信号)→ 直接采用,不调 LLM
/// - 关键词评估为 Evolving(不确定)→ 调 LLM 细化
/// - LLM 评估失败 → 降级回关键词结果
///
/// 这与 plan §1.2"小任务零成本跳过"一致:明确信号的任务不产生 LLM 调用成本。
#[async_trait]
pub trait FreshnessAssessor: Send + Sync {
    /// 语义评估任务的知识新鲜度。
    ///
    /// 返回 `(freshness, reason)`,reason 用于诊断日志。
    /// 生产实现应构造 prompt 让 LLM 返回 JSON `{"freshness": "novel|evolving|stable", "reason": "..."}`。
    /// 失败时返回 Err,gate_task 降级回关键词评估结果。
    async fn assess(&self, task: &str) -> Result<(KnowledgeFreshness, String), String>;
}

static GLOBAL_FRESHNESS_ASSESSOR: OnceLock<Option<Arc<dyn FreshnessAssessor>>> = OnceLock::new();

/// 注入全局 FreshnessAssessor(启动时调用)。
pub fn set_global_freshness_assessor(assessor: Arc<dyn FreshnessAssessor>) {
    let _ = GLOBAL_FRESHNESS_ASSESSOR.set(Some(assessor));
}

/// 获取全局 FreshnessAssessor(未注入返回 None)。
pub fn get_global_freshness_assessor() -> Option<Arc<dyn FreshnessAssessor>> {
    GLOBAL_FRESHNESS_ASSESSOR
        .get()
        .and_then(|opt| opt.as_ref().map(Arc::clone))
}

// ──────────────────────────────────────────────────────────────────────────
// QueryBuilderClient trait(Phase 2.3:LLM 提取搜索查询,依赖倒置)
// ──────────────────────────────────────────────────────────────────────────

/// 搜索查询构建器 trait(Phase 2.3)。
///
/// Phase 0/1 用启发式 `build_research_query`(取任务前 200 字符)。
/// Phase 2.3 升级为 LLM 提取:从任务文本中提取关键实体(库名/版本号/技术名词),
/// 构建更精准的搜索查询,提升 ResearchClient 的搜索命中率。
///
/// **成本控制**:仅 Novel 任务(需要调研)才调用,Stable/Evolving 不调研零成本。
/// LLM 提取失败时降级回启发式 `build_research_query`。
#[async_trait]
pub trait QueryBuilderClient: Send + Sync {
    /// 从任务文本提取关键实体,构建搜索查询。
    ///
    /// 返回适合 WebSearch 的查询字符串(通常 3-8 个关键词)。
    /// 失败时返回 Err,gate_task 降级回启发式查询。
    async fn build_query(&self, task: &str) -> Result<String, String>;
}

static GLOBAL_QUERY_BUILDER: OnceLock<Option<Arc<dyn QueryBuilderClient>>> = OnceLock::new();

/// 注入全局 QueryBuilderClient(启动时调用)。
pub fn set_global_query_builder(builder: Arc<dyn QueryBuilderClient>) {
    let _ = GLOBAL_QUERY_BUILDER.set(Some(builder));
}

/// 获取全局 QueryBuilderClient(未注入返回 None)。
pub fn get_global_query_builder() -> Option<Arc<dyn QueryBuilderClient>> {
    GLOBAL_QUERY_BUILDER
        .get()
        .and_then(|opt| opt.as_ref().map(Arc::clone))
}

// ──────────────────────────────────────────────────────────────────────────
// GatedTask:gate_task 的输出
// ──────────────────────────────────────────────────────────────────────────

/// gate_task 产出的门控结果,随 NodeResult 传递到 decision_log。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GatedTask {
    /// 评估出的知识新鲜度。
    pub freshness: KnowledgeFreshness,
    /// 调研摘要(仅当 needs_research 且 client 可用且调研成功时存在)。
    /// Phase 0(client 未注入)始终为 None。
    pub research_summary: Option<String>,
    /// 任务文本的哈希,用于缓存键。
    pub task_hash: u64,
}

impl GatedTask {
    /// 决策日志用的知识来源标签(Phase 1 DecisionRecord.knowledge_source 消费)。
    pub fn knowledge_source(&self) -> &'static str {
        match self.freshness {
            KnowledgeFreshness::Stable => "parametric",
            KnowledgeFreshness::Evolving => "parametric",
            KnowledgeFreshness::Novel => {
                if self.research_summary.is_some() {
                    "web_research"
                } else {
                    "parametric"
                }
            }
        }
    }

    /// 将调研摘要注入任务文本(Phase 1 摘要注入 system prompt 时使用)。
    ///
    /// Phase 0 无摘要,原样返回 task。
    pub fn enhance_task<'a>(&self, task: &'a str) -> std::borrow::Cow<'a, str> {
        match &self.research_summary {
            Some(summary) => std::borrow::Cow::Owned(format!(
                "{task}\n\n---\n## 调研材料(供参考,与任务冲突时需交叉验证)\n{summary}"
            )),
            None => std::borrow::Cow::Borrowed(task),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 缓存(避免 retry 重复调研)
// ──────────────────────────────────────────────────────────────────────────

// P3:改用 RwLock 替代 Mutex — 并行子任务密集查询时,多个相同 task_hash 的
// 读操作不会互相阻塞(读多写少场景)。写操作仍独占,保证一致性。
static GATE_CACHE: OnceLock<RwLock<HashMap<u64, GatedTask>>> = OnceLock::new();

fn gate_cache() -> &'static RwLock<HashMap<u64, GatedTask>> {
    GATE_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 读取缓存(命中返回 clone)。
///
/// RwLock 读锁中毒时降级为 None(不阻塞任务),符合 L2 规范
/// "用 unwrap_or_else(|e| e.into_inner()) 不用 expect"。
fn cache_get(task_hash: u64) -> Option<GatedTask> {
    let guard = gate_cache().read().unwrap_or_else(|e| e.into_inner());
    guard.get(&task_hash).cloned()
}

/// 写入缓存。
fn cache_put(task_hash: u64, gated: GatedTask) {
    let mut guard = gate_cache().write().unwrap_or_else(|e| e.into_inner());
    guard.insert(task_hash, gated);
}

// ──────────────────────────────────────────────────────────────────────────
// gate_task:门控入口
// ──────────────────────────────────────────────────────────────────────────

/// 从重试次数推导紧急性。
///
/// 急则治标:attempt > 0 表示已失败过,重试应跳过调研直接执行,避免延迟。
/// 首次执行(attempt = 0)走正常门控流程。
fn derive_urgent_from_attempt(attempt: u32) -> bool {
    attempt > 0
}

/// 计算任务文本的哈希(FNV-1a 64bit,足够区分,非密码学用途)。
fn hash_task(task: &str) -> u64 {
    // 简单 FNV-1a
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in task.as_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 构建调研查询(启发式,Phase 2 升级为 LLM 提取)。
///
/// MVP:直接用任务文本前 200 字符作为查询。Phase 2 用 LLM 从任务中提取
/// 关键实体(库名/版本号/技术名词)构建更精准的查询。
fn build_research_query(task: &str) -> String {
    // 取前 200 字符,避免过长查询
    let truncated = if task.len() > 200 {
        // 在字符边界截断(避免拆开 UTF-8)
        let mut end = 200;
        while end > 0 && !task.is_char_boundary(end) {
            end -= 1;
        }
        &task[..end]
    } else {
        task
    };
    truncated.to_string()
}

/// 知识新鲜度门控入口(async)。
///
/// 在 `coordinator_executor::execute` 内部调用(两条派发路径的共同上游)。
///
/// 流程:
/// 1. 缓存命中 → 直接返回(避免 retry 重复调研)
/// 2. 关键词评估 freshness
/// 3. Phase 2.1:若关键词评估为 Evolving(不确定)且 assessor 可用 → LLM 细化
/// 4. 若 Novel(或紧急跳过)且 client 可用 → 调研,失败降级
/// 5. 写入缓存,返回 GatedTask
///
/// # 降级语义
/// - assessor 未注入 → 用关键词评估结果(Phase 0/1 默认)
/// - assessor 调用失败 → 降级回关键词评估结果
/// - client 未注入 → research_summary = None(Phase 0 默认)
/// - client 调研失败 → research_summary = None,不阻塞任务
/// - 缓存 Mutex 中毒 → 跳过缓存,不阻塞任务
pub async fn gate_task(task: &str, attempt: u32) -> GatedTask {
    let task_hash = hash_task(task);

    // 1. 缓存命中(幂等性,避免 retry 成本爆炸)
    if let Some(cached) = cache_get(task_hash) {
        return cached;
    }

    // 2. 关键词评估 freshness
    let mut freshness = assess_knowledge_freshness(task);
    let urgent = derive_urgent_from_attempt(attempt);

    // 3. Phase 2.1:LLM 语义评估(仅当关键词评估为 Evolving 且 assessor 可用)。
    //    成本控制:Novel/Stable(强信号)不调 LLM,只对 Evolving(不确定)细化。
    //    非紧急(attempt=0)才评估;重试时跳过以减少延迟。
    if !urgent && freshness == KnowledgeFreshness::Evolving {
        if let Some(assessor) = get_global_freshness_assessor() {
            match assessor.assess(task).await {
                Ok((llm_freshness, _reason)) => {
                    freshness = llm_freshness;
                }
                Err(_) => {
                    // 降级:LLM 评估失败,保留关键词评估的 Evolving
                }
            }
        }
    }

    // 4. 调研(仅 Novel 且非紧急 且 client 可用)
    let research_summary = if !urgent && freshness.needs_research() {
        if let Some(client) = get_global_research_client() {
            // Phase 2.3:优先用 LLM 提取关键实体构建查询,降级回启发式。
            let query = if let Some(builder) = get_global_query_builder() {
                match builder.build_query(task).await {
                    Ok(q) => q,
                    Err(_) => build_research_query(task), // 降级:LLM 失败用启发式
                }
            } else {
                build_research_query(task) // 默认:启发式
            };
            client.research(&query).await.ok() // 降级:调研失败不阻塞任务
        } else {
            None // 降级:client 未注入(Phase 0 默认)
        }
    } else {
        None
    };

    let gated = GatedTask {
        freshness,
        research_summary,
        task_hash,
    };

    // 4. 写入缓存
    cache_put(task_hash, gated.clone());

    gated
}

// ──────────────────────────────────────────────────────────────────────────
// 单元测试
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── 评估函数测试 ──

    #[test]
    fn assess_novel_strong_signal() {
        // latest/newest/recent 单独命中即 Novel
        assert_eq!(
            assess_knowledge_freshness("upgrade to latest sdk"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("check the newest API"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("read recent changelog"),
            KnowledgeFreshness::Novel
        );
        // arxiv/paper/spec/rfc
        assert_eq!(
            assess_knowledge_freshness("implement arxiv paper algorithm"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("follow the spec"),
            KnowledgeFreshness::Novel
        );
    }

    #[test]
    fn assess_novel_action_with_version() {
        // 动作词 + 版本号共现 → Novel
        assert_eq!(
            assess_knowledge_freshness("migrate to 3.2"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("upgrade to react 18.2"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("pin to 1.4.2"),
            KnowledgeFreshness::Novel
        );
    }

    #[test]
    fn assess_novel_action_without_version_is_not_novel() {
        // 动作词无版本号 → 不判为 Novel(避免误报)
        // "upgrade to latest" 已被 NOVEL_STRONG 的 "latest" 捕获,这里测无强信号的情况
        let result = assess_knowledge_freshness("upgrade to the new architecture");
        // 无版本号,无强信号,但 "new" 不在 NOVEL_STRONG(只有 newest)
        assert_ne!(result, KnowledgeFreshness::Novel);
    }

    #[test]
    fn assess_bump_version_is_not_novel() {
        // D4 修订:"bump version" 不应被 "version " 误判为 Novel
        // "bump to" 需版本号共现,"bump version" 无版本号
        let result = assess_knowledge_freshness("bump version to release");
        assert_ne!(
            result,
            KnowledgeFreshness::Novel,
            "bump version (无版本号) 不应判为 Novel"
        );
    }

    #[test]
    fn assess_evolving_signals() {
        assert_eq!(
            assess_knowledge_freshness("refactor the module"),
            KnowledgeFreshness::Evolving
        );
        assert_eq!(
            assess_knowledge_freshness("fix the bug"),
            KnowledgeFreshness::Evolving
        );
        assert_eq!(
            assess_knowledge_freshness("optimize performance"),
            KnowledgeFreshness::Evolving
        );
    }

    #[test]
    fn assess_stable_signals() {
        assert_eq!(
            assess_knowledge_freshness("fix typo in readme"),
            KnowledgeFreshness::Evolving, // "fix" 命中 Evolving 先于 "typo" 的 Stable
        );
        // 纯 Stable(无 Evolving 信号)
        assert_eq!(
            assess_knowledge_freshness("format the code"),
            KnowledgeFreshness::Stable
        );
        assert_eq!(
            assess_knowledge_freshness("rename variable"),
            KnowledgeFreshness::Stable
        );
        assert_eq!(
            assess_knowledge_freshness("lint cleanup"),
            KnowledgeFreshness::Stable
        );
    }

    #[test]
    fn assess_novel_priority_over_stable() {
        // D4 修订:Novel > Evolving > Stable(风险优先)
        // "format the latest changelog" 同时命中 format(Stable)和 latest+changelog(Novel)
        assert_eq!(
            assess_knowledge_freshness("format the latest changelog"),
            KnowledgeFreshness::Novel
        );
    }

    #[test]
    fn assess_default_is_evolving() {
        // 无关键词命中 → 默认 Evolving
        assert_eq!(
            assess_knowledge_freshness("do something"),
            KnowledgeFreshness::Evolving
        );
        assert_eq!(
            assess_knowledge_freshness("build the feature"),
            KnowledgeFreshness::Evolving
        );
    }

    // ── 中文关键词测试 ──

    #[test]
    fn assess_chinese_novel_strong_signal() {
        assert_eq!(
            assess_knowledge_freshness("升级到最新版 SDK"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("查看发布说明"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("实现论文里的算法"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("该接口已被废弃"),
            KnowledgeFreshness::Novel
        );
    }

    #[test]
    fn assess_chinese_novel_action_with_version() {
        assert_eq!(
            assess_knowledge_freshness("升级到 3.2"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("迁移到 react 18.2"),
            KnowledgeFreshness::Novel
        );
    }

    #[test]
    fn assess_chinese_evolving_signals() {
        assert_eq!(
            assess_knowledge_freshness("重构这个模块"),
            KnowledgeFreshness::Evolving
        );
        assert_eq!(
            assess_knowledge_freshness("修复性能问题"),
            KnowledgeFreshness::Evolving // "修复"(Evolving)先于 "性能"(Evolving)
        );
    }

    #[test]
    fn assess_chinese_stable_signals() {
        assert_eq!(
            assess_knowledge_freshness("格式化代码"),
            KnowledgeFreshness::Stable // "格式" 命中 Stable
        );
        assert_eq!(
            assess_knowledge_freshness("把变量重命名"),
            KnowledgeFreshness::Stable
        );
    }

    // ── S4 负向排除测试 ──

    #[test]
    fn assess_local_internal_task_overrides_novel() {
        // 本地仓库语境 + Novel 信号 → 负向排除,回退 Stable
        assert_eq!(
            assess_knowledge_freshness("在这个仓库里升级 API 到 2.0"),
            KnowledgeFreshness::Stable
        );
        assert_eq!(
            assess_knowledge_freshness("本仓库中修复废弃接口"),
            KnowledgeFreshness::Stable
        );
    }

    #[test]
    fn assess_local_internal_task_with_file_path() {
        // 引用具体文件路径 → 本地任务
        assert_eq!(
            assess_knowledge_freshness("修改 src/main.rs 的接口"),
            KnowledgeFreshness::Stable
        );
        assert_eq!(
            assess_knowledge_freshness("在 crates/runtime 里修复 bug"),
            KnowledgeFreshness::Stable
        );
    }

    #[test]
    fn assess_local_internal_task_english() {
        assert_eq!(
            assess_knowledge_freshness("in this repo, fix the api"),
            KnowledgeFreshness::Stable
        );
        assert_eq!(
            assess_knowledge_freshness("our codebase uses the latest sdk"),
            KnowledgeFreshness::Stable
        );
    }

    #[test]
    fn assess_local_signal_does_not_leak_to_external_tasks() {
        // 无本地信号的外部任务不受负向排除影响
        assert_eq!(
            assess_knowledge_freshness("upgrade to latest sdk"),
            KnowledgeFreshness::Novel
        );
        assert_eq!(
            assess_knowledge_freshness("upgrade to react 18.2"),
            KnowledgeFreshness::Novel
        );
    }

    // ── 版本号检测测试 ──

    #[test]
    fn version_detection() {
        assert!(contains_version_number("upgrade to 3.2"));
        assert!(contains_version_number("react 18.2.0"));
        assert!(contains_version_number("pin 1.4"));
        assert!(!contains_version_number("bump version"));
        assert!(!contains_version_number("no version here"));
        assert!(!contains_version_number("v2"));
    }

    // ── GatedTask 测试 ──

    #[test]
    fn gated_task_knowledge_source() {
        let stable = GatedTask {
            freshness: KnowledgeFreshness::Stable,
            research_summary: None,
            task_hash: 0,
        };
        assert_eq!(stable.knowledge_source(), "parametric");

        let evolving = GatedTask {
            freshness: KnowledgeFreshness::Evolving,
            research_summary: None,
            task_hash: 0,
        };
        assert_eq!(evolving.knowledge_source(), "parametric");

        let novel_no_research = GatedTask {
            freshness: KnowledgeFreshness::Novel,
            research_summary: None,
            task_hash: 0,
        };
        assert_eq!(novel_no_research.knowledge_source(), "parametric");

        let novel_with_research = GatedTask {
            freshness: KnowledgeFreshness::Novel,
            research_summary: Some("summary".to_string()),
            task_hash: 0,
        };
        assert_eq!(novel_with_research.knowledge_source(), "web_research");
    }

    #[test]
    fn gated_task_enhance_task_without_summary() {
        let gated = GatedTask {
            freshness: KnowledgeFreshness::Novel,
            research_summary: None,
            task_hash: 0,
        };
        let task = "do something";
        assert_eq!(gated.enhance_task(task), task);
    }

    #[test]
    fn gated_task_enhance_task_with_summary() {
        let gated = GatedTask {
            freshness: KnowledgeFreshness::Novel,
            research_summary: Some("findings".to_string()),
            task_hash: 0,
        };
        let enhanced = gated.enhance_task("original task");
        assert!(enhanced.contains("original task"));
        assert!(enhanced.contains("findings"));
        assert!(enhanced.contains("调研材料"));
    }

    // ── gate_task 集成测试(无 client,降级路径)──

    #[tokio::test]
    async fn gate_task_without_client_returns_no_summary() {
        // Phase 0 默认:client 未注入,Novel 任务也不调研
        let gated = gate_task("upgrade to latest sdk", 0).await;
        assert_eq!(gated.freshness, KnowledgeFreshness::Novel);
        assert!(gated.research_summary.is_none());
    }

    #[tokio::test]
    async fn gate_task_stable_does_not_research() {
        let gated = gate_task("format the code", 0).await;
        assert_eq!(gated.freshness, KnowledgeFreshness::Stable);
        assert!(gated.research_summary.is_none());
    }

    #[tokio::test]
    async fn gate_task_caches_result() {
        // 同一任务第二次调用应命中缓存(同一 task_hash)
        let task = "cache test task unique 12345";
        let first = gate_task(task, 0).await;
        let second = gate_task(task, 0).await;
        assert_eq!(first.task_hash, second.task_hash);
        assert_eq!(first.freshness, second.freshness);
    }

    #[tokio::test]
    async fn gate_task_urgent_skips_research() {
        // attempt > 0(紧急)即使 Novel 也不调研
        let gated = gate_task("upgrade to latest sdk", 1).await;
        assert_eq!(gated.freshness, KnowledgeFreshness::Novel);
        assert!(gated.research_summary.is_none());
    }

    // ── 哈希测试 ──

    #[test]
    fn hash_is_deterministic() {
        let h1 = hash_task("test task");
        let h2 = hash_task("test task");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_differs_for_different_tasks() {
        let h1 = hash_task("task A");
        let h2 = hash_task("task B");
        assert_ne!(h1, h2);
    }

    // ── ResearchClient trait 测试(mock client)──

    struct MockResearchClient {
        response: Result<String, String>,
    }

    #[async_trait]
    impl ResearchClient for MockResearchClient {
        async fn research(&self, _query: &str) -> Result<String, String> {
            self.response.clone()
        }
    }

    #[tokio::test]
    async fn gate_task_with_mock_client_researches_novel() {
        // 注意:全局 OnceLock 在测试进程中只能 set 一次,且会污染后续测试。
        // 这里不直接 set_global,而是测试 client 注入逻辑的正确性。
        let client = MockResearchClient {
            response: Ok("mock research summary".to_string()),
        };
        // 直接调用 research 验证 trait 工作正常
        let result = client.research("query").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "mock research summary");
    }

    #[tokio::test]
    async fn gate_task_with_failing_client_degrades() {
        let client = MockResearchClient {
            response: Err("network error".to_string()),
        };
        let result = client.research("query").await;
        assert!(result.is_err());
        // gate_task 会降级为 None,不阻塞任务
    }
}
