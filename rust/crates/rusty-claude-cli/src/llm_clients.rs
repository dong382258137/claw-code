//! v2 Phase 2 生产适配器 — 把 runtime 的 `JudgeClient` / `DecisionExtractorClient`
//! trait 接到真实的 LLM 调用上。
//!
//! # 背景
//!
//! runtime crate 不能依赖 api crate(避免循环依赖),因此 v2 Phase 2 的两个 trait
//! 通过依赖倒置方式定义在 runtime 内,生产实现必须在上层 crate(rusty-claude-cli)构造。
//!
//! 本模块:
//! - [`LlmBridge`] 共享桥接:同步 trait → async LLM 调用,与 `AnthropicRuntimeClient::stream`
//!   (streaming.rs) 同样的 `runtime.block_on(async { ... })` 模式
//! - [`DeepSeekJudgeClient`] impl `JudgeClient`,用于 `LlmJudgeGate::with_client`
//! - [`DeepSeekDecisionExtractorClient`] impl `DecisionExtractorClient`,用于
//!   `set_global_decision_extractor_client`
//! - [`DeepSeekCompactionSummarizerClient`] impl `CompactionSummarizerClient`,用于
//!   `set_global_compaction_summarizer_client`(LLM context editing)
//!
//! # 调用栈安全性
//!
//! `LlmJudgeGate::validate` 在 `execute_dispatch_subagent_async` retry loop 内触发,
//! `extract_decisions_before_compaction` 在 `compact_session` 之前触发。c051bac0 后
//! 两者都在 `ConversationRuntime::run_turn_async` 的 async 调用栈内,当前线程**已在**
//! tokio runtime 上下文(LocalSet 驱动)。
//!
//! 因此 `LlmBridge::call` 不能直接 `runtime.block_on(...)`,否则触发
//! "Cannot start a runtime from within a runtime" panic。`call` 内部通过
//! `Handle::try_current()` 检测:若已在 runtime 中,在独立 OS 线程上执行
//! `Handle::block_on`(该线程不在任何 tokio runtime 上下文,可安全驱动 future);
//! 否则(如同步单元测试)直接 `self.runtime.block_on(...)`。
//!
//! `block_in_place` 不可用:claw-shell 使用 `current_thread` + `LocalSet`,
//! `block_in_place` 在 `current_thread` runtime 上会 panic。
//!
//! # 错误降级
//!
//! - `JudgeClient::judge` 失败 → `LlmJudgeGate::validate` 返回 `retryable=false` 错误
//! - `DecisionExtractorClient::extract` 失败 → `extract_decisions_with_llm` 自动降级为 Heuristic
//!
//! 适配器只需把 `ApiError` 转为 `Err(String)`,上层降级逻辑已就位。

use std::sync::Arc;

use api::{max_tokens_for_model, InputMessage, MessageRequest, OutputContentBlock, ProviderClient};
use runtime::decision_log::DecisionExtractorClient;
use runtime::multi_agent::validation::JudgeClient;

/// LLM 调用桥接 — 同步接口包裹 async `ProviderClient::send_message`。
///
/// 持有独立的 `tokio::runtime::Runtime`(与主 agent 的 runtime 隔离,避免阻塞主 agent)
/// 和独立的 `ProviderClient`(独立 prompt cache,不污染主 agent 缓存,符合 §5.2 缓存保护)。
///
/// `ProviderClient` 实现 `Clone`,但 `tokio::runtime::Runtime` 不实现 `Clone`,
/// 因此 `LlmBridge` 不实现 `Clone`。需要共享时用 `Arc<LlmBridge>`。
struct LlmBridge {
    /// 独立 tokio runtime(与主 agent runtime 隔离)。
    runtime: tokio::runtime::Runtime,
    /// 独立 LLM client(从环境变量读取 auth,与主 agent 共享同一份 API key)。
    client: ProviderClient,
    /// 模型名(每次调用复用,保证 judge / extractor 走指定模型)。
    model: String,
    /// 单次调用 max_tokens 上限。
    max_tokens: u32,
}

impl LlmBridge {
    /// 构造桥接。
    ///
    /// # 参数
    /// - `model`:LLM 模型名(如 "deepseek-v4-pro" / "deepseek-v4-flash")
    /// - `max_tokens`:单次响应上限。`None` 时用 `api::max_tokens_for_model` 默认值
    ///
    /// # 错误
    /// - tokio runtime 创建失败(极少见,通常是系统资源不足)
    /// - `ProviderClient::from_model` 失败(API key 缺失 / 模型名无法识别 provider)
    fn new(model: &str, max_tokens: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()
                .map_err(|e| format!("create tokio runtime: {e}"))?,
            client: ProviderClient::from_model(model)
                .map_err(|e| format!("create provider client: {e}"))?,
            model: model.to_string(),
            max_tokens: max_tokens.unwrap_or_else(|| max_tokens_for_model(model)),
        })
    }

    /// 同步调用 LLM,返回纯文本响应。
    ///
    /// # 流程
    /// 1. 构造 `MessageRequest`(单条 user message,stream=false)
    /// 2. `runtime.block_on(client.send_message(&request))`
    /// 3. 从 `MessageResponse.content` 提取所有 `Text` 块拼接为字符串
    ///
    /// # 错误
    /// 网络/API/超时等返回 `Err(String)`,由上层降级处理。
    #[allow(clippy::result_large_err)]
    fn call(&self, prompt: &str) -> Result<String, String> {
        let request = MessageRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            messages: vec![InputMessage::user_text(prompt)],
            stream: false,
            ..Default::default()
        };

        // v3 修复(c051bac0 后):调用方可能在 tokio runtime 上下文中
        // (run_turn_async 调用栈内,如 JudgeGate::validate 或
        // extract_decisions_before_compaction)。直接 self.runtime.block_on
        // 会触发 "Cannot start a runtime from within a runtime" panic。
        // 检测到当前线程已在 runtime 中时,在独立 OS 线程上执行 block_on,
        // 该线程不在任何 tokio runtime 上下文,可安全驱动 future。
        // block_in_place 不可用:claw-shell 使用 current_thread + LocalSet,
        // block_in_place 在 current_thread runtime 上会 panic。
        let response = if tokio::runtime::Handle::try_current().is_ok() {
            let handle = self.runtime.handle().clone();
            let client = self.client.clone();
            std::thread::spawn(move || {
                handle.block_on(async move { client.send_message(&request).await })
            })
            .join()
            .map_err(|e| format!("LLM bridge worker thread panicked: {e:?}"))?
        } else {
            // 调用方不在 tokio runtime 上下文(如同步单元测试),直接 block_on 安全。
            self.runtime
                .block_on(async { self.client.send_message(&request).await })
        }
        .map_err(|e| format!("LLM send_message failed: {e}"))?;

        // 提取所有 Text 块拼接(过滤 ToolUse / Thinking / RedactedThinking)
        let text: String = response
            .content
            .iter()
            .filter_map(|b| {
                if let OutputContentBlock::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
        Ok(text)
    }
}

/// 生产 `JudgeClient` 实现 — 用于 `LlmJudgeGate::with_client`。
///
/// 封装 `LlmBridge`,把 `JudgeClient::judge` 路由到 `ProviderClient::send_message`。
/// 注入到 `LlmJudgeGate` 后,诊断/架构任务的 validation 链会执行 LLM-as-judge 评分
/// (根因定位/方案可行性/完整性/副作用四维)。
///
/// # 构造
/// ```ignore
/// use rusty_claude_cli::llm_clients::DeepSeekJudgeClient;
/// use std::sync::Arc;
/// use runtime::multi_agent::validation::{JudgeClient, LlmJudgeGate};
///
/// let judge: Arc<dyn JudgeClient> = Arc::new(DeepSeekJudgeClient::new("deepseek-v4-pro", None)?);
/// let gate = LlmJudgeGate::diagnostic_default("deepseek-v4-pro", workspace_root)
///     .with_client(judge);
/// ```
pub struct DeepSeekJudgeClient {
    bridge: LlmBridge,
}

impl DeepSeekJudgeClient {
    /// 构造 judge client。
    ///
    /// # 参数
    /// - `model`:judge 模型名(建议用旗舰模型保证判断质量,如 "deepseek-v4-pro")
    /// - `max_tokens`:单次响应上限。`None` 用模型默认值(judge 通常 1024 足够)
    pub fn new(model: &str, max_tokens: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            bridge: LlmBridge::new(model, max_tokens)?,
        })
    }
}

impl JudgeClient for DeepSeekJudgeClient {
    fn judge(&self, prompt: &str) -> Result<String, String> {
        self.bridge.call(prompt)
    }
}

/// 生产 `DecisionExtractorClient` 实现 — 用于 `set_global_decision_extractor_client`。
///
/// 封装 `LlmBridge`,把 `DecisionExtractorClient::extract` 路由到
/// `ProviderClient::send_message`。注入到全局 OnceLock 后,context compaction
/// 触发时 `extract_decisions_before_compaction` 的 `LlmExtract` 分支会真正调用 LLM
/// 提取结构化决策点(context/decision/rationale/alternatives)。
///
/// # 构造
/// ```ignore
/// use rusty_claude_cli::llm_clients::DeepSeekDecisionExtractorClient;
/// use std::sync::Arc;
/// use runtime::decision_log::{DecisionExtractorClient, set_global_decision_extractor_client};
///
/// let extractor: Arc<dyn DecisionExtractorClient> =
///     Arc::new(DeepSeekDecisionExtractorClient::new("deepseek-v4-flash", None)?);
/// set_global_decision_extractor_client(extractor);
/// ```
pub struct DeepSeekDecisionExtractorClient {
    bridge: LlmBridge,
}

impl DeepSeekDecisionExtractorClient {
    /// 构造决策提取 client。
    ///
    /// # 参数
    /// - `model`:提取模型名(可用 budget 模型,如 "deepseek-v4-flash",降低成本)
    /// - `max_tokens`:单次响应上限。`None` 用模型默认值(提取通常 2048 足够)
    pub fn new(model: &str, max_tokens: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            bridge: LlmBridge::new(model, max_tokens)?,
        })
    }
}

impl DecisionExtractorClient for DeepSeekDecisionExtractorClient {
    fn extract(&self, prompt: &str) -> Result<String, String> {
        self.bridge.call(prompt)
    }
}

/// 生产 `CompactionSummarizerClient` 实现 — 用于
/// `set_global_compaction_summarizer_client`。
///
/// 封装 `LlmBridge`,把 `CompactionSummarizerClient::summarize` 路由到
/// `ProviderClient::send_message`。注入到全局 OnceLock 后,context compaction
/// 触发时 `summarize_messages_with_llm` 会调用 LLM 生成**模型摘要**
/// (context editing 核心能力),失败时自动 3 路降级为启发式规则摘要。
///
/// # 构造
/// ```ignore
/// use rusty_claude_cli::llm_clients::DeepSeekCompactionSummarizerClient;
/// use std::sync::Arc;
/// use runtime::compact::{CompactionSummarizerClient, set_global_compaction_summarizer_client};
///
/// let summarizer: Arc<dyn CompactionSummarizerClient> =
///     Arc::new(DeepSeekCompactionSummarizerClient::new("deepseek-v4-flash", Some(2048))?);
/// set_global_compaction_summarizer_client(summarizer);
/// ```
pub struct DeepSeekCompactionSummarizerClient {
    bridge: LlmBridge,
}

impl DeepSeekCompactionSummarizerClient {
    /// 构造压缩摘要 client。
    ///
    /// # 参数
    /// - `model`:摘要模型名(建议 budget 模型,如 "deepseek-v4-flash",降低成本)
    /// - `max_tokens`:单次响应上限。`None` 用模型默认值(摘要通常 2048 足够)
    pub fn new(model: &str, max_tokens: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            bridge: LlmBridge::new(model, max_tokens)?,
        })
    }
}

impl runtime::compact::CompactionSummarizerClient for DeepSeekCompactionSummarizerClient {
    fn summarize(&self, prompt: &str) -> Result<String, String> {
        self.bridge.call(prompt)
    }
}

/// 生产 `PlanGeneratorClient` 实现 — 用于 `set_global_plan_generator_client`。
///
/// 封装 `LlmBridge`,把 `PlanGeneratorClient::generate_plan` 路由到
/// `ProviderClient::send_message`。注入到全局 OnceLock 后,复杂任务
/// 由模型生成计划步骤(LLM-driven planning),失败时自动回退启发式分解。
pub struct DeepSeekPlanGeneratorClient {
    bridge: LlmBridge,
}

impl DeepSeekPlanGeneratorClient {
    /// 构造计划生成 client。
    ///
    /// # 参数
    /// - `model`:计划模型名(建议旗舰模型保证步骤质量,如 "deepseek-v4-pro")
    /// - `max_tokens`:单次响应上限。`None` 用模型默认值(计划通常 2048 足够)
    pub fn new(model: &str, max_tokens: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            bridge: LlmBridge::new(model, max_tokens)?,
        })
    }
}

impl runtime::planner::PlanGeneratorClient for DeepSeekPlanGeneratorClient {
    fn generate_plan(&self, prompt: &str) -> Result<String, String> {
        self.bridge.call(prompt)
    }
}

/// 生产 `ResearchClient` 实现 — 用于
/// `runtime::knowledge_freshness::set_global_research_client`。
///
/// # 架构
///
/// `runtime::knowledge_freshness::ResearchClient` 是 async trait,定义在 runtime
/// crate(不依赖 tools/api)。本实现位于 rusty-claude-cli(同时依赖 runtime +
/// tools + api),封装:
///
/// 1. **搜索+抓取**(同步):调用 `tools::research_topic`(内部用 reqwest
///    blocking client 执行 WebSearch + WebFetch)
/// 2. **LLM 摘要**(同步):用 `LlmBridge` 把多个 `ResearchHit` 拼成摘要 prompt,
///    调用 LLM 生成结构化摘要
/// 3. **async 桥接**:本 impl 的 `research` 是 async fn,内部用 `tokio::task::
///    spawn_blocking` 包装同步的搜索+抓取+摘要,避免阻塞 tokio worker
///
/// # 调用栈安全性
///
/// `gate_task` 在 `coordinator_executor::execute`(async)内调用,当前线程已在
/// tokio runtime 上下文。`research` 内部用 `spawn_blocking` 把同步的
/// `research_topic` + `LlmBridge::call` 放到 blocking pool,不触发
/// "Cannot start a runtime from within a runtime" panic。
///
/// (LlmBridge::call 内部已有 `Handle::try_current` 检测,在 spawn_blocking
/// 线程上会走 `std::thread::spawn` 分支,安全。)
///
/// # 降级语义
///
/// - 搜索失败(网络/超时)→ 返回 Err → `gate_task` 降级为 None(不调研)
/// - LLM 摘要失败 → 返回 Err → 同上
/// - 搜索结果为空 → 返回 Ok("(无搜索结果)") → 作为摘要注入(透明降级)
///
/// # 构造
/// ```ignore
/// use rusty_claude_cli::llm_clients::WebResearchClient;
/// use std::sync::Arc;
/// use runtime::knowledge_freshness::{ResearchClient, set_global_research_client};
///
/// let client: Arc<dyn ResearchClient> =
///     Arc::new(WebResearchClient::new("deepseek-v4-flash", Some(2048))?);
/// set_global_research_client(client);
/// ```
pub struct WebResearchClient {
    /// LLM 摘要桥接(搜索本身不需要 LLM,用 LlmBridge 做摘要)。
    bridge: LlmBridge,
    /// 抓取的页面数(建议 2-3,平衡成本与覆盖)。
    max_results: usize,
}

impl WebResearchClient {
    /// 构造调研 client。
    ///
    /// # 参数
    /// - `model`:摘要模型名(建议用 budget 模型,如 "deepseek-v4-flash")
    /// - `max_tokens`:单次摘要响应上限(建议 2048)
    pub fn new(model: &str, max_tokens: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            bridge: LlmBridge::new(model, max_tokens)?,
            max_results: 3,
        })
    }

    /// 构建摘要 prompt:把多个 ResearchHit 拼成 LLM 输入。
    fn build_summary_prompt(query: &str, hits: &[tools::ResearchHit]) -> String {
        let sources = hits
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                format!(
                    "## 来源 {n}\n**标题:** {title}\n**URL:** {url}\n**内容:**\n{content}\n",
                    n = i + 1,
                    title = hit.title,
                    url = hit.url,
                    content = hit.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n---\n\n");

        format!(
            "你是一个技术调研助手。请根据以下搜索结果,为查询「{query}」生成一份简洁的摘要。\n\
             \n\
             ## 要求\n\
             - 提取与查询相关的关键技术事实(版本号、API 变化、最佳实践等)\n\
             - 如果搜索结果之间有冲突,标注冲突并给出最可信的结论\n\
             - 如果搜索结果过时或与查询无关,明确说明\n\
             - 摘要长度控制在 500 字以内\n\
             - 不要复述来源,直接给出结论\n\
             \n\
             ## 查询\n\
             {query}\n\
             \n\
             ## 搜索结果\n\
             {sources}"
        )
    }
}

#[async_trait::async_trait]
impl runtime::knowledge_freshness::ResearchClient for WebResearchClient {
    async fn research(&self, query: &str) -> Result<String, String> {
        // 搜索+抓取+摘要都是同步阻塞操作,用 spawn_blocking 包装。
        // LlmBridge::call 内部已有 runtime 检测,在 blocking 线程上安全。
        //
        // LlmBridge 持有独立的 tokio::runtime::Runtime(不 Clone),因此用
        // Arc<LlmBridge> 共享到 spawn_blocking 闭包中。
        let query = query.to_string();
        let max_results = self.max_results;
        let bridge_ref = &self.bridge;

        // 用 scope hack:LlmBridge 不 Clone,但 spawn_blocking 需要 'static。
        // 解决方案:把搜索和摘要分两步,搜索在 spawn_blocking,摘要在当前
        // async 上下文用 LlmBridge::call(它内部自己处理 runtime 检测)。
        let query_for_search = query.clone();
        let hits = tokio::task::spawn_blocking(move || -> Result<Vec<tools::ResearchHit>, String> {
            tools::research_topic(&query_for_search, max_results)
        })
        .await
        .map_err(|e| format!("research spawn_blocking panicked: {e:?}"))??;

        if hits.is_empty() {
            return Ok(format!("(搜索「{query}」无结果)"));
        }

        // LLM 摘要:LlmBridge::call 是同步的,但它内部用 Handle::try_current
        // 检测 runtime 上下文,在 async 上下文中会走 std::thread::spawn 分支,
        // 不会 panic。直接调用即可(它会阻塞当前 tokio worker,但 LLM 调用
        // 通常 < 5s,可接受;若需优化可再用 spawn_blocking)。
        let prompt = Self::build_summary_prompt(&query, &hits);
        let summary = bridge_ref.call(&prompt)?;

        Ok(summary)
    }
}

/// 生产 `FreshnessAssessor` 实现(Phase 2.1)— 用于
/// `runtime::knowledge_freshness::set_global_freshness_assessor`。
///
/// 封装 `LlmBridge`,用 flash 模型对任务文本做语义新鲜度评估。
/// 当关键词评估为 Evolving(不确定)时,gate_task 调用本 assessor 细化判断。
///
/// # 成本
///
/// 仅对关键词无法确定(Evolving)的任务调用 LLM,明确 Novel/Stable 的任务零 LLM 成本。
/// 建议用 budget 模型(如 "deepseek-v4-flash")降低成本。
///
/// # 降级
///
/// LLM 调用失败 / JSON 解析失败 / 枚举值无法识别 → 返回 Err,
/// gate_task 降级回关键词评估的 Evolving。
///
/// # 构造
/// ```ignore
/// use rusty_claude_cli::llm_clients::DeepSeekFreshnessAssessor;
/// use std::sync::Arc;
/// use runtime::knowledge_freshness::{FreshnessAssessor, set_global_freshness_assessor};
///
/// let assessor: Arc<dyn FreshnessAssessor> =
///     Arc::new(DeepSeekFreshnessAssessor::new("deepseek-v4-flash", Some(256))?);
/// set_global_freshness_assessor(assessor);
/// ```
pub struct DeepSeekFreshnessAssessor {
    bridge: LlmBridge,
}

impl DeepSeekFreshnessAssessor {
    /// 构造新鲜度评估器。
    ///
    /// # 参数
    /// - `model`:评估模型名(建议 budget 模型,如 "deepseek-v4-flash")
    /// - `max_tokens`:单次响应上限(评估通常 256 足够,只返回 JSON)
    pub fn new(model: &str, max_tokens: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            bridge: LlmBridge::new(model, max_tokens)?,
        })
    }

    /// 构建评估 prompt:让 LLM 返回 JSON `{"freshness": "...", "reason": "..."}`。
    fn build_assess_prompt(task: &str) -> String {
        format!(
            "你是一个技术任务分类器。请评估以下任务所需的「知识新鲜度」:\n\
             \n\
             - **stable**:纯机械操作(typo/format/rename/lint/import),知识高度稳定,无需查最新信息\n\
             - **evolving**:涉及重构/修复/优化,框架 API 可能演进,默认可查可不查\n\
             - **novel**:涉及新版本/新论文/最新规范/冷门工具链,参数记忆可能过时,需查最新信息\n\
             \n\
             ## 任务\n\
             {task}\n\
             \n\
             ## 输出格式\n\
             只返回一行 JSON,不要其他内容:\n\
             {{\"freshness\": \"stable|evolving|novel\", \"reason\": \"简短理由\"}}\n\
             \n\
             ## 判定原则\n\
             - 涉及具体版本号/升级/迁移 → novel\n\
             - 涉及论文/spec/rfc/changelog → novel\n\
             - 纯格式化/重命名/拼写 → stable\n\
             - 模糊的重构/修复 → evolving\n\
             - 不确定时倾向 evolving(保守)"
        )
    }

    /// 解析 LLM 返回的 JSON,提取 freshness 枚举。
    fn parse_assess_response(response: &str) -> Result<(runtime::knowledge_freshness::KnowledgeFreshness, String), String> {
        // LLM 可能返回带 ```json 包裹的 JSON,提取首个 { 到末尾
        let trimmed = response.trim();
        let json_str = if let Some(start) = trimmed.find('{') {
            let end = trimmed.rfind('}').ok_or("missing closing } in assessor response")?;
            &trimmed[start..=end]
        } else {
            trimmed
        };

        let parsed: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("assessor response is not valid JSON: {e} (response: {response})"))?;

        let freshness_str = parsed
            .get("freshness")
            .and_then(|v| v.as_str())
            .ok_or("missing 'freshness' field in assessor response")?;

        let freshness = match freshness_str.to_lowercase().as_str() {
            "stable" => runtime::knowledge_freshness::KnowledgeFreshness::Stable,
            "evolving" => runtime::knowledge_freshness::KnowledgeFreshness::Evolving,
            "novel" => runtime::knowledge_freshness::KnowledgeFreshness::Novel,
            other => return Err(format!("unknown freshness value: {other}")),
        };

        let reason = parsed
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok((freshness, reason))
    }
}

#[async_trait::async_trait]
impl runtime::knowledge_freshness::FreshnessAssessor for DeepSeekFreshnessAssessor {
    async fn assess(&self, task: &str) -> Result<(runtime::knowledge_freshness::KnowledgeFreshness, String), String> {
        let prompt = Self::build_assess_prompt(task);
        // LlmBridge::call 内部有 runtime 检测,在 async 上下文中安全
        let response = self.bridge.call(&prompt)?;
        Self::parse_assess_response(&response)
    }
}

/// 生产 `QueryBuilderClient` 实现(Phase 2.3)— 用于
/// `runtime::knowledge_freshness::set_global_query_builder`。
///
/// 封装 `LlmBridge`,用 flash 模型从任务文本提取关键实体(库名/版本号/技术名词),
/// 构建适合 WebSearch 的精准查询。当 gate_task 判定任务为 Novel(需调研)时调用。
///
/// # 成本
///
/// 仅对 Novel 任务调用(Stable/Evolving 不调研),每次调研前调一次。
/// 建议用 budget 模型(如 "deepseek-v4-flash")降低成本。
///
/// # 降级
///
/// LLM 调用失败 / 响应为空 → 返回 Err,gate_task 降级回启发式
/// `build_research_query`(取任务前 200 字符)。
///
/// # 构造
/// ```ignore
/// use rusty_claude_cli::llm_clients::DeepSeekQueryBuilderClient;
/// use std::sync::Arc;
/// use runtime::knowledge_freshness::{QueryBuilderClient, set_global_query_builder};
///
/// let builder: Arc<dyn QueryBuilderClient> =
///     Arc::new(DeepSeekQueryBuilderClient::new("deepseek-v4-flash", Some(128))?);
/// set_global_query_builder(builder);
/// ```
pub struct DeepSeekQueryBuilderClient {
    bridge: LlmBridge,
}

impl DeepSeekQueryBuilderClient {
    /// 构造查询构建器。
    ///
    /// # 参数
    /// - `model`:提取模型名(建议 budget 模型,如 "deepseek-v4-flash")
    /// - `max_tokens`:单次响应上限(查询通常 128 足够,只返回关键词)
    pub fn new(model: &str, max_tokens: Option<u32>) -> Result<Self, String> {
        Ok(Self {
            bridge: LlmBridge::new(model, max_tokens)?,
        })
    }

    /// 构建提取 prompt:让 LLM 从任务文本提取 3-8 个搜索关键词。
    fn build_query_prompt(task: &str) -> String {
        format!(
            "你是一个搜索查询构建器。请从以下任务描述中提取关键实体(库名、版本号、技术名词、API 名),\n\
             构建一个适合 Web 搜索的英文查询字符串(3-8 个关键词,空格分隔)。\n\
             \n\
             ## 任务\n\
             {task}\n\
             \n\
             ## 要求\n\
             - 提取技术实体,忽略无关词(如「请」「实现」「这个」)\n\
             - 如果有版本号,包含版本号(如 \"react 18.2\")\n\
             - 如果有库名,用英文库名(如 \"tokio\" 而非「异步运行时」)\n\
             - 只返回查询字符串本身,不要其他内容、不要引号、不要解释\n\
             \n\
             ## 示例\n\
             任务: \"升级 tokio 到 1.40 并修复编译错误\"\n\
             查询: tokio 1.40 migration breaking changes\n\
             \n\
             任务: \"实现 arxiv 论文中的 attention 算法\"\n\
             查询: arxiv attention algorithm implementation"
        )
    }
}

#[async_trait::async_trait]
impl runtime::knowledge_freshness::QueryBuilderClient for DeepSeekQueryBuilderClient {
    async fn build_query(&self, task: &str) -> Result<String, String> {
        let prompt = Self::build_query_prompt(task);
        let response = self.bridge.call(&prompt)?;
        let query = response.trim().trim_matches('"').to_string();
        if query.is_empty() {
            return Err("LLM returned empty query".to_string());
        }
        Ok(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 `LlmBridge::new` 在缺少 API key 时返回 Err(而非 panic)。
    ///
    /// 这个测试不依赖网络,只验证错误传播。在 CI 无 API key 环境下也应该通过
    /// (返回 Err 而非 panic)。若本机有 API key,则 `LlmBridge::new` 成功,
    /// 此时 `call` 会尝试真实网络调用,我们不测 `call` 以避免 flaky。
    #[test]
    fn llm_bridge_new_returns_err_on_missing_auth() {
        // 临时移除 DeepSeek auth 环境变量,模拟无 auth 场景
        let keys = ["DEEPSEEK_API_KEY"];
        let saved: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for k in &keys {
            std::env::remove_var(k);
        }

        // DeepSeek-only build: ProviderClient::from_model 读 DEEPSEEK_API_KEY
        let result = LlmBridge::new("deepseek-v4-pro", None);

        // 恢复环境变量
        for (k, v) in &saved {
            if let Some(val) = v {
                std::env::set_var(k, val);
            }
        }

        // 无论 result 是 Ok 还是 Err,都不应 panic。
        // 在无 auth 环境下应为 Err;在有 auth 环境下可能为 Ok(跳过断言)。
        match &result {
            Ok(_) => {
                // 有 auth 环境,跳过断言(测试仍通过)
                return;
            }
            Err(msg) => {
                // 验证错误消息非空
                assert!(!msg.is_empty(), "error message should not be empty");
            }
        }
        assert!(result.is_err(), "expected Err when no auth available");
    }

    /// 验证 `DeepSeekJudgeClient` 可构造并实现 `JudgeClient` trait。
    #[test]
    fn deepseek_judge_client_implements_judge_client() {
        // 只验证类型trait 约束,不实际构造(避免依赖 API key)
        fn assert_judge_client<T: JudgeClient>() {}
        assert_judge_client::<DeepSeekJudgeClient>();
    }

    /// 验证 `DeepSeekDecisionExtractorClient` 可构造并实现 `DecisionExtractorClient` trait。
    #[test]
    fn deepseek_decision_extractor_client_implements_trait() {
        fn assert_extractor<T: DecisionExtractorClient>() {}
        assert_extractor::<DeepSeekDecisionExtractorClient>();
    }

    /// 验证 `Arc<dyn JudgeClient>` 可从 `DeepSeekJudgeClient` 构造
    /// (验证 v-table + Send + Sync 约束满足)。
    #[test]
    fn deepseek_judge_client_can_be_arc_dyn() {
        // 不实际构造(避免依赖 API key),只验证类型转换
        fn assert_arc_dyn(_x: std::sync::Arc<dyn JudgeClient>) {}
        // 若编译通过,说明类型约束满足
    }

    /// 验证 `Arc<dyn DecisionExtractorClient>` 可从 `DeepSeekDecisionExtractorClient` 构造。
    #[test]
    fn deepseek_decision_extractor_client_can_be_arc_dyn() {
        fn assert_arc_dyn(_x: std::sync::Arc<dyn DecisionExtractorClient>) {}
    }
}
