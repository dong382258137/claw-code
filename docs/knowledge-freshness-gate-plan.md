# 知识新鲜度门控 (Knowledge Freshness Gate) 设计方案 v3

> 状态:设计方案 v3(实现可行性评审修订版 — 修复 D1-D4/M1-M5 共 9 项断层)
> 相关讨论:中医辨证"四诊合参"理念 → LLM 问题求解方法论
> 评审:2026-07 PRO 评审 + 代码事实逐条验证(v2,见 §10 S1-S8);
>       2026-08 实现可行性评审(v3,见 §10 D1-D4 / M1-M5)

## 1. 背景与动机

### 1.1 问题

LLM 在接到任务时,默认从**参数记忆**(训练数据)中直接生成方案,而非先检索外部信息。
这在两类场景下会导致任务质量显著下降:

1. **知识截止后演进的领域**:新框架 API、新库版本、最新论文方法、近期最佳实践。
   模型会用旧知识**自信地**生成错误答案(置信度校准差,无法区分"我知道"与"我在模式匹配")。
2. **超出训练分布的任务**:冷门工具链、私有协议、快速变化的生态。

这与中医"勤求古训,博采众方"的原则相悖:好医生遇到疑难杂证会翻医书、请会诊,
而不是只凭记忆开方。现有机制中"Wheel reinvention: Search first"是**被动触发**的
(怀疑轮子被重复发明才搜);本方案将其升级为**事前门控**(开方前先查方)。

### 1.2 目标

- 在**子 agent 执行链的 system prompt 构造点**之前,自动评估其"知识新鲜度需求"
- 对易变/未知领域任务,强制插入**前置搜索节点**,产出调研摘要注入执行上下文
- 在决策日志中记录**知识来源**,形成"查来的方子 vs 背出来的方子"的疗效对比闭环
- 小任务零成本跳过(避免研究瘫痪),大任务先博采再执行

### 1.3 非目标

- 不改变 `WebSearch`/`WebFetch` 工具本身的实现
- 不做"一律先搜"的强制——保留"急则治标"(紧急任务先执行后补查)的分诊语义
- MVP 不做 LLM 语义评估(见 §7 分阶段),只做零成本的启发式评估

## 2. 现状分析(已逐条验证,§10 附证据)

| 组件 | 位置 | 现状 | 验证 |
|---|---|---|---|
| `TaskComplexity` | `api/.../model_tier.rs` + `runtime/.../multi_agent/mod.rs` | Simple/Diagnostic/Architectural → 模型路由,无知识维度 | ✅ |
| `WebSearch` 工具 | `tools/src/lib.rs:4068` `execute_web_search`(**同步 fn**) | 支持 query/域名过滤/去重,truncate(8) | ✅ |
| `Subagent` 扩展字段 | `runtime/.../multi_agent/mod.rs:89` | 已有 model/complexity/max_attempts/cost_limit,`#[serde(default)]` 先例 | ✅ |
| `CoordinatorExecutor::execute` | `.../coordinator_executor.rs:145` | **`&self` + `&DagNode` 不可变引用**,runner 签名 `Fn(String,String)` | ✅ |
| `SubagentDispatcher` | `.../subagent_dispatcher.rs:21` | 从 `run_subagent_turn` 提取,**system prompt 构造点** (`dispatch_impl`) | ✅ |
| `run_subagent_turn_with_model` | `runtime/src/conversation.rs:3127` | **会话内派发路径**(主战场,spawn_parallel_subagents 等工具走这里) | ✅ |
| `DagNode` | `.../dag/types.rs` | **全字段显式构造**(无 `..Default::default()`),加字段破坏所有构造点 | ⚠️ |
| `assess_complexity` | `runtime/src/planner/mod.rs:112` | 启发式关键词评估,本方案的评估层参考模板 | ✅ |
| `HIGH_RISK_KEYWORDS` | `runtime/src/planner/mod.rs:57` | **private**(非 pub),复用需先 pub 化 | ⚠️ |
| `DecisionExtractorClient` | `runtime/src/decision_log.rs:858` | **依赖倒置先例**:trait + `OnceLock` + `set_global_*` + private `global_*()` | ✅ |
| `PRAGMA user_version` | `runtime/src/decision_log.rs:780-793` | schema 迁移机制已存在(v1→v2),加 v3 可行 | ✅ |
| runtime 依赖 | `runtime/Cargo.toml` | **不依赖 tools/api** → 依赖倒置必要 | ✅ |

## 3. 核心设计:KnowledgeFreshness 评估

### 3.1 新枚举

```rust
/// 任务涉及领域的"知识新鲜度需求" — 决定是否需要前置搜索。
/// 与 TaskComplexity(任务复杂性)正交:复杂性管"用什么模型",新鲜度管"要不要先查"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeFreshness {
    /// 成熟稳定领域:参数记忆足够,零搜索(格式化、已知 API、通用重构模式)。
    Stable,
    /// 演进中领域:可能有过时风险,前置一次搜索(新版本 API、最佳实践变化)。
    Evolving,
    /// 未知/新鲜领域:参数记忆大概率未覆盖,强制前置搜索 + 多源交叉(新论文、新工具链)。
    Novel,
}
```

### 3.2 评估函数(启发式,MVP)

```rust
/// 评估任务的知识新鲜度需求。
///
/// 判定顺序(短路,**风险优先** — 漏判 Novel 成本高于误判 Novel):
/// 1. 先判 Novel:命中"时滞敏感实体 + 动作词共现"(版本升级动作 + 版本号 / 论文 / changelog)
///    → Novel
/// 2. 再判 Stable:命中"纯机械操作"关键词(格式化/重命名/注释/拼写/已知重构)
///    且**未命中** Novel 信号 → Stable
/// 3. 其余 → Evolving
///
/// 返回附带 reason(用于诊断日志,同 assess_complexity 风格)。
pub fn assess_knowledge_freshness(task: &str) -> (KnowledgeFreshness, String) { ... }
```

关键词表(中英文,参考 `assess_complexity`/`HIGH_RISK_KEYWORDS` 风格):

```rust
/// 纯机械操作信号 → Stable(仅当未命中 NOVEL 时生效)
const STABLE_KEYWORDS: &[&str] = &[
    "format", "rename", "reformat", "typo", "comment", "indent",
    "格式化", "重命名", "注释", "拼写", "缩进", "排版",
];

/// 时滞敏感实体信号 → Novel
/// D4 修正(评审):收紧为"动作词 + 实体词共现"模式,降低误报。
/// - 单独 "version"/"latest"/"sdk" 不触发(避免 "bump version"、"format with latest prettier" 误报)
/// - 要求动作词(upgrade/migrate/...)与版本号/实体词共现,或论文/changelog 等强信号单独命中
const NOVEL_ACTION_WORDS: &[&str] = &[
    "upgrade to", "migrate to", "port to", "update to",
    "升级到", "迁移到", "移植到", "更新到",
];

const NOVEL_ENTITY_WORDS: &[&str] = &[
    // 版本号模式(正则,需配合版本号检测):\d+\.\d+(\.\d+)?
];

/// 强 Novel 信号:单独命中即 Novel(无需共现)
const NOVEL_STRONG_SIGNALS: &[&str] = &[
    // 论文/学术(强信号,训练数据天然滞后)
    "arxiv", "论文", "publication", "white paper",
    // 外部更新日志(强信号,明确指向外部知识源)
    "changelog", "release notes", "更新日志", "发布说明",
    // 新工具/冷门协议
    "unknown protocol", "experimental feature", "beta release",
    "实验性功能", "测试版发布",
];

/// 判定逻辑(伪代码):
/// 1. 含 NOVEL_STRONG_SIGNALS 任一 → Novel
/// 2. 含 NOVEL_ACTION_WORDS 任一 且 含版本号(正则 \d+\.\d+)→ Novel
/// 3. 否则含 STABLE_KEYWORDS 任一 → Stable
/// 4. 其余 → Evolving
```

> **D4 修正(评审)**:判定顺序由"Stable > Novel"改为"**Novel > Stable**"(风险优先)。
> 理由:漏判 Novel(用过时知识自信地错答)成本远高于误判 Novel(多搜一次)。
> 同时 NOVEL_KEYWORDS 拆为"动作词 + 实体词共现"+ "强信号单独命中"两层,避免
> "bump version"、"format with latest prettier"、"bump sdk version" 等常见误报。

> **S4 修正(评审)**:关键词命中是**候选信号**而非最终裁决。评估函数最终结果还受
> **负向排除**约束:任务明显是本地/内部操作(如"在这个仓库里…"、"项目内…"、
> 引用具体文件路径)时,即使含 "api"/"sdk" 也回退 Stable——避免对内部 API 误触发搜索。

### 3.3 风险维度(复用现有,需先 pub 化)

**Risk 来源(明确,避免与 assess_complexity 冲突)**:
- **直接复用 `planner::HIGH_RISK_KEYWORDS`**(`delete`/`drop`/`production`/`security`/`migrate` 等)
  作为风险分级,**不**复用 `assess_complexity` 的 `ComplexityAssessment` 输出。
- 理由:`assess_complexity` 同时编码"复杂性 + 风险"两维(`Complex { high_risk }`),
  本方案的 Risk 维度只需风险信号,直接读关键词表更清晰,避免拆解 `ComplexityAssessment`
  造成的语义混淆。两者关系:并行运行,各司其职 —— `assess_complexity` 决定用哪个模型,
  本方案决定要不要先搜索。
- **前置条件**:将 `HIGH_RISK_KEYWORDS` 改为 `pub const`(目前是 private,见 §10 S6)。

两维组合形成门控决策表:

| Freshness \ Risk | 低风险 | 高风险 |
|---|---|---|
| **Stable** | 直接执行(零搜索) | 直接执行 + 本地 `search_decisions` 检索 |
| **Evolving** | 前置搜索 ×1(flash 总结) | 前置搜索 ×1 + 验证门禁已有 |
| **Novel** | **强制前置搜索 ×2~3 + 多源交叉** | 前置搜索 + 验证门禁 + `knowledge_source=web_research` 落日志 |

> 语义对应:"急则治标" = 高风险且紧急时允许跳过搜索先执行,事后补查(§5.3)。
> **M3 修正(评审)**:Risk 维度只读 `HIGH_RISK_KEYWORDS`,**不读** `assess_complexity`,
> 两者并行不冲突。

## 4. 实现架构(评审修订:运行时注入,不改数据模型)

### 4.1 核心决策(v2 修订)

**不往 `DagNode`/`Subagent` 加字段**。理由(§10 S2/S3):
- `DagNode` 全字段显式构造,加字段会破坏 `coordinator_executor.rs` 测试 `sample_node()`、
  `types.rs` 测试 `node()` 及所有构造点;
- 门控是**运行时行为**,不是数据模型;搜索结果无需持久化到节点定义。

门控以**独立模块 + 全局注入**方式存在,在两条派发路径的**汇聚点**
(system prompt 构造之前)被调用。

### 4.2 新模块 `knowledge_freshness.rs`(runtime 内,与 decision_log.rs 同级)

```rust
/// 前置调研 client — 依赖倒置,生产实现由上层 crate 注入。
/// runtime 不直接依赖 tools crate(已验证 Cargo.toml 无 tools 依赖)。
///
/// D1 修正(评审):trait 改为 async。两条派发路径均为 async fn
/// (`run_subagent_turn_with_model` / `dispatch_impl`),同步阻塞网络 IO 会卡住
/// tokio worker;且 `dispatch_impl` 内部已用 `std::thread::spawn` 隔离
/// `client.stream()` 的 `block_on`(subagent_dispatcher.rs:98-106),同步 research
/// 在此处会与隔离模式冲突。`async-trait` 已在 runtime 依赖中。
#[async_trait::async_trait]
pub trait ResearchClient: Send + Sync {
    /// 执行一次调研查询,返回摘要文本。
    /// 生产实现内部可:WebSearch → WebFetch top N → LLM 摘要拼接。
    async fn research(&self, query: &str) -> Result<String, String>;
}

static GLOBAL_RESEARCH_CLIENT: OnceLock<Option<Arc<dyn ResearchClient>>> = OnceLock::new();

pub fn set_global_research_client(client: Arc<dyn ResearchClient>) { ... }
fn global_research_client() -> Option<&'static Arc<dyn ResearchClient>> { ... }

/// 门控结果。
///
/// D3 修正(评审):GatedTask 必须**随执行结果一起传递到落库点**,不能只用于
/// 构造 system prompt 后丢弃。freshness 字段用于映射 knowledge_source(§6.1)。
#[derive(Clone)]
pub struct GatedTask {
    pub freshness: KnowledgeFreshness,
    /// 增强后的任务文本(搜索成功时含调研摘要;否则原文)。
    pub task: String,
    pub research_summary: Option<String>,
    /// 未注册 client 时的降级标志(记录日志,不阻塞)。
    pub degraded: bool,
    /// 急则治标旁路标记(从 retry_policy 推导,见 §5.3)。
    pub deferred_research: bool,
    pub reason: String,
}

impl GatedTask {
    /// 映射到 DecisionRecord.knowledge_source(§6.1 传递链)。
    pub fn knowledge_source(&self) -> &'static str {
        if self.deferred_research {
            "deferred_research"
        } else if self.research_summary.is_some() {
            "web_research"
        } else if self.degraded {
            "parametric_memory"
        } else {
            "parametric_memory"
        }
    }
}

/// 核心门控函数:评估 → 按决策表搜索 → 返回增强任务。
///
/// D2 修正(评审):`urgent` 参数从外部签名移除,改为从 `retry_policy` 推导
/// (见 §5.3)。调用方传入 `attempt: u32`(当前重试次数,0 = 首次),
/// gate_task 内部按 attempt 判断是否进入急则治标旁路。
///
/// D1 修正:函数改为 async(因 ResearchClient::research 是 async)。
pub async fn gate_task(task: &str, attempt: u32) -> GatedTask { ... }
```

生产实现由 `rusty-claude-cli` 注入:封装 `tools::execute_web_search` +
`WebFetch` 抓取 top 结果 + LLM 摘要拼接。**未注册时门控静默降级**
(记录 `degraded = true`,不阻塞任务)。

### 4.3 插入点:两条派发路径的汇聚点

子 agent 有**两条**派发路径(§10 S1 修正):

```
路径 A(会话内,主战场):
  spawn_parallel_subagents / dispatch_subagent 工具
    → ConversationRuntime::run_subagent_turn_with_model   (conversation.rs:3127)
    → 构造 system_prompt
路径 B(DAG 调度):
  DagScheduler → CoordinatorExecutor::execute → SubagentRunner
    → SubagentDispatcher::dispatch_impl                    (subagent_dispatcher.rs:55)
    → 构造 system_prompt (SystemPromptSplit::from_sections)
```

两条路径最终都走到 **system prompt 构造点**。门控在此处之前调用
`gate_task(&task, urgent)`,将增强后的 task 传入 system prompt 构造。

> **为什么不在 CoordinatorExecutor::execute 挂**:该方法是 `&self` + `&DagNode`
> 不可变引用,伪代码中"修改 node.task"不可编译;且只覆盖路径 B。

### 4.4 摘要注入方式

构造 system prompt 时,在任务 section 后追加"前置调研材料"section
(与 `SystemPromptSplit::from_sections` 的 sections 模式天然契合):

```text
## 前置调研材料(知识新鲜度门控产出)
<research_summary>
```

> **M2 修正(评审)**:子 agent 被明确要求:若调研材料与任务描述或参数记忆冲突,
> **不得无条件以调研材料为准**,而是:
> 1. 标注冲突点并说明两侧依据;
> 2. 对关键事实**交叉验证**(至少 2 个独立来源一致才采纳);
> 3. 若调研材料明显与任务前提矛盾(如任务要求 v2 API,调研说已废弃),回退到
>    参数记忆并在输出中提示"调研材料可能过时,建议人工复核"。
>
> 理由:WebSearch 结果可能返回过时信息、错误信息、对抗性内容,盲目以搜索结果为准
> 可能比参数记忆更糟。交叉验证 + 冲突显式标注比"无条件优先"更安全。

## 5. 门控执行流

```
gate_task(task, attempt):
  ├─ freshness, reason = assess_knowledge_freshness(task)   // §3.2
  ├─ urgent = derive_urgent_from_attempt(attempt)            // §5.3
  ├─ if urgent → 跳过搜索,deferred_research = true,返回原文
  ├─ 查 task_hash 缓存(§5.2):命中 → 直接返回缓存的 GatedTask
  ├─ client = global_research_client()
  ├─ if client 未注册 → degraded = true,返回原文(不阻塞)
  ├─ match freshness:
  │    Stable   → 返回原文(零搜索)
  │    Evolving → query = build_research_query(task); summary = client.research(query).await
  │    Novel    → query = build_research_query(task)
  │               summary = client.research(query).await
  │               summary += client.research(query_variant).await   // 多源交叉
  ├─ task_enhanced = append_research_section(task, summary)
  ├─ 写入 task_hash 缓存(§5.2)
  └─ return GatedTask
```

> **D1 修正**:gate_task 为 `async fn`,所有 `client.research()` 调用 `.await`。
> **D2 修正**:`urgent` 不再是外部参数,由 `attempt` 推导。

### 5.1 build_research_query(启发式)

从任务文本提取 3~8 个关键词作为搜索 query:过滤停用词 + 保留实体词
(含大写缩写、版本号、库名)。MVP 用简单分词 + 白名单。

### 5.2 幂等性与缓存(M1 修正)

`CoordinatorExecutor::execute` 在 retry 时会被 scheduler **重复调用**;
`run_subagent_turn` 同理。因此:
- `assess_knowledge_freshness` 是纯函数(无副作用),重复评估零成本;
- `research()` 有网络成本 → **MVP 必须加 task_hash 缓存**,避免 retry 重复搜索
  (Novel 任务每次重试搜 2-3 次,成本放大 N 倍,违反 §1.2"小任务零成本跳过"原则)。

```rust
use std::collections::HashMap;
use std::sync::Mutex;

/// task_hash → GatedTask 缓存。MVP 用最简实现(无 LRU 淘汰,进程生命周期内有效)。
/// 线程安全:Mutex 保护,ResearchClient 实现需自行保证内部无共享可变状态。
static GATE_CACHE: Mutex<Option<HashMap<u64, GatedTask>>> = Mutex::new(None);

fn cache_get(task_hash: u64) -> Option<GatedTask> {
    GATE_CACHE.lock().unwrap_or_else(|e| e.into_inner())
        .as_ref()?.get(&task_hash).cloned()
}

fn cache_put(task_hash: u64, gated: GatedTask) {
    let mut guard = GATE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(task_hash, gated);
}
```

> **M1 修正(评审)**:原方案"MVP 不跨 retry 复用"在 Novel 场景下成本爆炸
> (retry × 3 次搜索)。改为 `Mutex<HashMap<u64, GatedTask>>` 极简缓存,
> task_hash 用 `sha2`(已在 runtime 依赖中)对 task 文本哈希。
> 缓存键不含 attempt:同一 task 的 freshness 评估结果稳定,
> 首次评估后后续 retry 直接复用(包括 research_summary)。

### 5.3 紧急任务旁路(急则治标)— D2 修正

**urgent 来源**:不再作为 `gate_task` 的外部参数(原方案未定义来源),
改为从 `attempt: u32`(当前重试次数)**推导**:

```rust
/// 从重试次数推导是否进入急则治标旁路。
/// - attempt = 0(首次):不旁路,正常门控
/// - attempt >= 1(已重试):进入旁路,跳过搜索直接重试
///
/// 语义:首次失败后,任务已有时效压力(用户在等),此时再花时间搜索反而加剧延迟,
/// 不如直接重试;事后通过 deferred_research 标记提示补查。
fn derive_urgent_from_attempt(attempt: u32) -> bool {
    attempt >= 1
}
```

`attempt` 来源:
- 路径 A(`run_subagent_turn_with_model`):从 `max_attempts` 配置和当前 turn 计数推导
- 路径 B(`dispatch_impl` via `CoordinatorExecutor::execute`):scheduler 在 retry 时
  传入 attempt(已有 retry 循环,见 `retry_policy`)

旁路生效时:
- 跳过前置搜索,直接执行
- `GatedTask.deferred_research = true`
- `knowledge_source()` 返回 `"deferred_research"`(§6.1)
- 验证通过后输出提示:"本次未做前置调研,建议事后补查以校准方案"

## 6. 决策日志增强(knowledge_source)

### 6.1 DecisionRecord 新字段与传递链(D3 修正)

```rust
/// 方案的知识来源 — 用于统计"查来的 vs 背出来的"疗效差异。
pub knowledge_source: Option<String>, // parametric_memory | local_history | web_research | mixed | deferred_research
```

**传递链(D3 修正,关键)**:`gate_task` 在 system prompt 构造前调用,
`DecisionRecord` 在任务完成后落库,中间跨多个层级。明确传递路径:

```
gate_task(task, attempt) → GatedTask
  ↓ GatedTask.task 用于构造 system prompt
  ↓ GatedTask.freshness / deferred_research / research_summary 必须随执行结果传递
  ↓
路径 A: run_subagent_turn_with_model 返回值增加 GatedTask 字段
        → 调用方(ConversationRuntime)在 log_decision 时读取 gated.knowledge_source()
路径 B: dispatch_impl 返回 (result, GatedTask)
        → CoordinatorExecutor::execute 把 GatedTask 透传给 NodeResult
        → scheduler 在 log_decision 时读取 gated.knowledge_source()
```

**实现要点**:
- `GatedTask` 已 derive `Clone`(§4.2),可随 `NodeResult` 传递
- `NodeResult` 需增加 `gated: Option<GatedTask>` 字段(路径 B);
  路径 A 通过 `run_subagent_turn_with_model` 返回值携带
- `log_decision` 调用点读取 `gated.knowledge_source()` 写入 `DecisionRecord.knowledge_source`
- **若 GatedTask 丢失(传递链断裂)**:knowledge_source 写 `None`,
  不阻塞落库,但闭环统计会缺失该条记录(可观测,可修复)

> **D3 修正(评审)**:原方案只定义了字段,没定义传递链。结果可能是字段加了
> 但永远写不进去(类似 `assess_complexity` 结果在部分路径上未被消费)。
> 现明确 GatedTask 随执行结果传递,log_decision 调用点映射。

### 6.2 Schema 迁移 v3

`PRAGMA user_version = 3`,ALTER TABLE 增加 `knowledge_source TEXT`。
已验证:v1→v2 迁移机制存在(decision_log.rs:787-793),FTS5 触发器只索引
签名/假设/方案三字段,**加列不影响 FTS**。

### 6.3 闭环统计

`search_decisions` 输出增加 `source:` 行;后续可统计
`success_rate by knowledge_source`,反向校准评估阈值(§7 Phase 2)。

## 7. 分阶段实施

| Phase | 范围 | 交付物 | 成本 |
|---|---|---|---|
| **0(MVP)** | `knowledge_freshness.rs`(枚举+评估+`gate_task` async+trait async+降级+task_hash 缓存);`planner` 的 `HIGH_RISK_KEYWORDS` 改 `pub`;两条派发路径插入 `gate_task` 调用;`NodeResult` 加 `gated` 字段 + 传递链 | 可运行,零额外 LLM 调用 | ~450 行 |
| **1** | 上层注入 `ResearchClient` 生产实现(WebSearch + WebFetch + **LLM 摘要拼接 prompt 工程**);决策日志 `knowledge_source` + schema v3 | 端到端生效 | ~300 行 |
| **2** | LLM 语义评估(flash 评估新鲜度,替代关键词);success_rate by source 统计;`build_research_query` 升级为 LLM 提取 | 更准的评估 + 闭环 | ~250 行 |

MVP 优先:启发式评估零成本、不阻塞、可回退,与仓库 `DetectionStrategy::Heuristic`
的"MVP 启发式 → v2 LLM"演进路径一致。

> **M5 修正(评审)**:Phase 1 行数从 ~150 调整为 ~300。原估算未含:
> - LLM 摘要拼接的 prompt 工程(从 N 个网页文本提取要点,需设计 prompt 模板 + few-shot)
> - 摘要生成的错误处理(网页抓取失败、LLM 超时/空响应)
> - 摘要长度控制(避免注入 system prompt 后超 token 上限)
> - async ResearchClient 生产实现的 tokio 集成(D1)
> Phase 0 行数从 ~350 调整为 ~450,增量来自 task_hash 缓存(M1)+ NodeResult 传递链(D3)。

## 8. 文件改动清单(v2 修订 + D1/D2/D3/M1 修订)

| 文件 | 改动 |
|---|---|
| `rust/crates/runtime/src/knowledge_freshness.rs`(新) | 枚举 + 评估 + `gate_task`(async) + `ResearchClient` trait(async) + 全局注入 + task_hash 缓存(M1) + `derive_urgent_from_attempt`(D2) + 单测 |
| `rust/crates/runtime/src/planner/mod.rs` | `HIGH_RISK_KEYWORDS` 改为 `pub const`(§10 S6) |
| `rust/crates/runtime/src/conversation.rs` | `run_subagent_turn_with_model` 在构造 system prompt 前 `await gate_task`,返回值携带 `GatedTask`(D3 传递链路径 A) |
| `rust/crates/runtime/src/multi_agent/dag/subagent_dispatcher.rs` | `dispatch_impl` 在构造 system prompt 前 `await gate_task`,返回 `(result, GatedTask)`(D3 传递链路径 B) |
| `rust/crates/runtime/src/multi_agent/dag/coordinator_executor.rs` | `execute` 把 `GatedTask` 透传给 `NodeResult`(D3) |
| `rust/crates/runtime/src/multi_agent/dag/types.rs` | `NodeResult` 增加 `gated: Option<GatedTask>` 字段(D3,**会破坏构造点,需同步更新 `node()` 测试**) |
| `rust/crates/runtime/src/decision_log.rs` | `DecisionRecord.knowledge_source` + schema v3 + 输出 source + `log_decision` 调用点读取 `gated.knowledge_source()`(Phase 1) |
| `rust/crates/runtime/src/lib.rs` | 导出 `knowledge_freshness` 模块 |
| `rust/crates/rusty-claude-cli/src/llm_clients.rs`(或等价处) | 启动时 `set_global_research_client` 注入生产实现(Phase 1) |

**不动的文件**(评审修正):`dag/types.rs` 的 `DagNode`、`multi_agent/mod.rs` 的 `Subagent`——
不加字段,避免破坏构造点。`NodeResult` 加字段是 D3 传递链必需,会破坏 `types.rs`
测试 `node()` 构造点,需同步更新(影响范围可控,NodeResult 构造点少于 DagNode)。

## 9. 测试计划

**基础测试**:
1. **评估函数单测**:Stable/Novel/Evolving 三分类的典型正例/负例
   (复用 decision_log.rs 测试风格:中英文关键词、大小写不敏感、无匹配回退 Evolving)
2. **负向排除测试**:含 "api" 但属内部仓库任务 → 回退 Stable
3. **门控降级测试**:未注册 research client 时 Novel 任务仍能执行(不阻塞)
4. **摘要注入测试**:mock ResearchClient 返回固定摘要,验证构造的 system prompt 含调研 section
5. **attempt 旁路测试**(D2 修正):`attempt >= 1` 跳过搜索,
   `GatedTask.deferred_research = true`,`knowledge_source()` 返回 `deferred_research`
6. **schema 迁移测试**:v2 → v3 迁移后旧决策可读、新字段可写
7. **回归**:现有 DAG/subagent/conversation 单测全绿
   (DagNode 无改动;NodeResult 加字段需更新 `types.rs::node()` 测试构造点)

**判定顺序测试**(D4 修正):
8. **风险优先测试**:任务同时命中 Stable 和 Novel 信号(如"fix typo in arxiv paper")
   → 应判 Novel(不是 Stable)
9. **共现约束测试**:任务含 "upgrade to" 但无版本号 → 不判 Novel;
   含 "upgrade to 2.0" → 判 Novel
10. **强信号单独命中测试**:任务含 "arxiv" 单词(无动作词)→ 判 Novel
11. **常见误报回归测试**:"bump version"、"format with latest prettier"、
    "bump sdk version" → 应判 Stable(不误触发搜索)

**缓存测试**(M1 修正):
12. **task_hash 缓存命中测试**:同 task 调用 `gate_task` 两次,
    research client 只被调用一次(缓存命中)
13. **不同 task 不串味测试**:两个不同 task 的 GatedTask 互不干扰

**失败降级测试**(M4 补全):
14. **搜索失败降级**:mock client 返回 `Err` → `degraded = true`,任务原文执行不阻塞
15. **搜索超时降级**:mock client 模拟超时 → 同上
16. **搜索空结果降级**:mock client 返回空字符串 → `research_summary = Some("")`,
    task 不追加 research section

**传递链测试**(D3 修正):
17. **路径 A 传递链**:mock `run_subagent_turn_with_model`,验证返回值含 GatedTask,
    且 `log_decision` 被调用时 `knowledge_source` 字段正确写入
18. **路径 B 传递链**:mock `dispatch_impl` + `CoordinatorExecutor::execute`,
    验证 `NodeResult.gated` 字段被填充,scheduler 落库时 `knowledge_source` 正确
19. **传递链断裂容错**:人为丢弃 GatedTask → `knowledge_source` 写 `None`,
    不阻塞落库

**并发与集成测试**(M4 补全):
20. **并发安全测试**:多线程同时调 `gate_task`,`GATE_CACHE` Mutex 不中毒、不数据竞争
21. **路径 A 真实集成测试**:注册 mock client,跑完整 `run_subagent_turn_with_model` 流程,
    验证 system prompt 含调研 section + 返回值含 GatedTask
22. **路径 B 真实集成测试**:注册 mock client,跑完整 DAG 调度流程,
    验证 NodeResult.gated 透传到落库

**性能上限测试**(M4 补全):
23. **Novel ×3 搜索延迟上限**:mock client 每次延迟 500ms,
    验证 Novel 任务 `gate_task` 总耗时 < 2s(不卡死 DAG scheduler)
24. **缓存命中零延迟**:第二次同 task 调用 `gate_task` 耗时 < 1ms(纯内存查表)

## 10. 评审附录(PRO 评审 + 代码验证记录)

| # | 严重度 | 评审发现 | 处置 |
|---|---|---|---|
| S1 | 致命 | `execute(&self, &DagNode)` 不可变引用,伪代码改 node.task 不可编译;且子 agent 有两条派发路径,只挂 execute 覆盖一半 | v2 §4.3:门控移到两条路径的 system prompt 构造点 |
| S2 | 重要 | `DagNode` 全字段显式构造(无 `..Default::default()`),加字段破坏 `sample_node()`/`node()` 等所有构造点 | v2 §4.1:不加字段,运行时注入 |
| S3 | 重要 | `Subagent` 虽有 `#[serde(default)]` 先例,但加字段需改 spawn + 相关测试,非必需 | v2 §4.1:不加字段 |
| S4 | 重要 | 关键词评估误报:"api"/"version" 不必然需要搜索(内部 API) | v2 §3.2:增加负向排除(本地/内部任务回退 Stable) |
| S5 | 建议 | decision_log schema 迁移机制验证通过;FTS 触发器只索引指定列,加 knowledge_source 无影响 | v2 §6.2:维持方案 |
| S6 | 建议 | `HIGH_RISK_KEYWORDS` 是 private(planner/mod.rs:57),复用需先 pub | v2 §3.3:改 `pub const` |
| S7 | 建议 | `execute_web_search` 是同步 fn(tools/lib.rs:4068),ResearchClient trait 应匹配同步签名 | **D1 修正(评审)推翻本条**:trait 改 async。两条派发路径均为 async fn,同步阻塞会卡 tokio worker,且与 `dispatch_impl` 的 `std::thread::spawn` 隔离模式冲突 |
| S8 | 建议 | retry 时 execute 会被重复调用,若每次重搜成本高 | v2 §5.2:评估纯函数零成本;搜索一次性 |
| **D1** | **致命** | 同步 `research()` 在 async 上下文(`dispatch_impl`/`run_subagent_turn_with_model`)中阻塞 tokio worker;与 `subagent_dispatcher.rs:98-106` 的 `std::thread::spawn` 隔离模式冲突 | v3 §4.2:`ResearchClient` 改 `#[async_trait]`,`gate_task` 改 `async fn`;推翻 S7 的"同步签名"结论 |
| **D2** | **致命** | `urgent: bool` 参数来源未定义,两条派发路径签名均无此字段 | v3 §5.3:删除 `urgent` 参数,改为从 `attempt: u32`(重试次数)推导;`gate_task(task, attempt)` |
| **D3** | **致命** | `freshness → knowledge_source` 传递链未定义,字段可能永远写不进去 | v3 §6.1:`GatedTask` 随 `NodeResult`(路径 B)/返回值(路径 A)传递,`log_decision` 调用点映射;`NodeResult` 加 `gated` 字段 |
| **D4** | **重要** | 判定顺序 Stable > Novel 会导致漏判("fix typo in arxiv paper" → Stable);NOVEL_KEYWORDS 过宽("version"/"latest"/"sdk" 单独命中误报) | v3 §3.2:判定顺序改 Novel > Stable(风险优先);NOVEL 拆为"动作词+版本号共现"+"强信号单独命中"两层 |
| **M1** | **重要** | MVP"不跨 retry 复用"在 Novel 场景成本爆炸(retry × 3 次搜索) | v3 §5.2:加 `Mutex<HashMap<u64, GatedTask>>` task_hash 缓存,首次评估后 retry 直接复用 |
| **M2** | **建议** | "以调研材料为准"危险,WebSearch 结果可能过时/错误/对抗 | v3 §4.4:改为"交叉验证 + 冲突显式标注",关键事实需 ≥2 独立来源一致 |
| **M3** | **建议** | Risk 来源未明确,与 `assess_complexity` 的 `ComplexityAssessment` 关系不清 | v3 §3.3:Risk 只读 `HIGH_RISK_KEYWORDS`,不读 `assess_complexity`;两者并行不冲突 |
| **M4** | **建议** | 测试计划缺失败降级、并发、集成、性能测试 | v3 §9:测试从 7 项扩展到 24 项,覆盖 4 类新场景 |
| **M5** | **建议** | Phase 1 行数 ~150 未含 LLM 摘要 prompt 工程 + 错误处理 + 长度控制 | v3 §7:Phase 1 调整为 ~300,Phase 0 调整为 ~450 |

**验证证据**:本方案中所有"已验证 ✅"事实均来自实际读取代码
(runtime/Cargo.toml 无 tools 依赖;decision_log.rs:780-793 迁移机制;
conversation.rs:3127 会话派发;subagent_dispatcher.rs:55 system prompt 构造点等)。
