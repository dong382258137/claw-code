# P3 Phase 3: LLM 驱动的自进化 Harness 模块设计(优化版)

> **状态**: 设计文档(待评审)
> **前置条件**: P1(LogCompressor) + P2(Output Token Reduction) 已完成
> **论文依据**: GSME(arXiv:2607.13683) + Self-Harness + HASE(arXiv:2607.03935) + ERL(ICLR 2026) + Misevolution(arXiv:2509.26354)
> **优化原则**: 第一性原理驱动,删除过度设计,复用现有基础设施

---

## 一、设计目标

### 1.1 核心目标

让 Claw 在**不修改模型权重**的前提下,通过 LLM 驱动的闭环自进化,持续优化自身的 harness surface(系统提示 dynamic_sections),实现跨会话的能力积累。

### 1.2 量化目标

| 维度 | MVP(规则式) | Phase 3(LLM 驱动) | 提升来源 |
|------|-------------|-------------------|---------|
| 重复错误修复 turn 数 | 3-5 turn | 1-2 turn | lessons 检索注入 |
| 新会话冷启动 | 从零开始 | 继承历史教训 | harness_edits 持久化 |
| 未预见失败处理 | 7 种预定义模式 | LLM 归纳新 pathology | 混合 Proposer 动态生成 |
| 跨会话能力保持 | NOTEBOOK 文本 | 结构化 edits + 验证 | success_rate 学习环 |
| 整体任务效率提升 | 10-15% | 35-50% | GSME +9~15.5pp + ERL +7.8% |

### 1.3 设计约束(防 misevolution)

基于 Misevolution 论文(arXiv:2509.26354)的 99.3% 自我改进是有界自优化结论,本方案强制:

1. **Proposing/Crediting 分离**(GSME 核心创新):LLM 只提议,确定性代码归因
2. **外部信号门控**:所有 edit 必须关联 TaskSuccessRate(单一信号,已包含编译/测试/工具成功)
3. **保守晋升**:Candidate → Active 需通过两重门控 + success_rate > 0.7
4. **可回滚**:所有 edits 持久化为 SQLite,支持一键回滚

### 1.4 优化决策(基于第一性原理评估)

| 原方案 | 优化后 | 理由 |
|--------|--------|------|
| 三重门控(Validity + Activation + Significance) | **两重门控**(Validity + Significance) | Activation Gate 在小样本下信号弱,已被 Validity 覆盖 |
| 纯 LLM Proposer | **规则优先 + LLM 兜底** | 80% 常见错误被预定义模式覆盖,减少 70% LLM 调用 |
| 独立 JSON 持久化(GatedArchive) | **独立 SQLite 表**(共用 decision_log.db) | 复用 SQLite 事务 + 独立 schema,避免 JSON 崩溃风险 |
| 4 种外部信号 | **单一信号**(TaskSuccessRate) | TaskSuccessRate 是编译/测试/工具成功的上位概念,已包含信息 |
| 检索式注入(top-k) | **全量注入**(10 条上限) | 10 条 × 500 chars ≈ 1.5K tokens,占比 <15%,检索复杂度不划算 |
| 异步 tokio::spawn | **同步限频**(每 10 turn + 5s 超时) | 规则式路径零延迟,异步带来状态一致性难题 |
| 4 状态机 | **3 状态机**(Candidate/Active/Retired) | Rejected 和 RolledBack 无 actionable 区分价值 |
| 7 文件模块 | **3 文件模块** | 内聚到 evolution.rs + types.rs + mod.rs |
| 有状态 EvolutionCoordinator | **无状态函数** | 组件本身无状态,只需传入 trace + archive |

---

## 二、总体架构

### 2.1 两阶段闭环循环(基于 GSME + ERL)

```text
┌─────────────────────────────────────────────────────────────────┐
│                  自进化闭环(每 10 turn 触发一次)                  │
│                                                                  │
│  ┌────────────────────┐    ┌──────────────────────────────────┐ │
│  │ Stage 1            │    │ Stage 2                          │ │
│  │ Weakness Mining    │───>│ Mixed Proposer                   │ │
│  │ (确定性代码)        │    │ (规则优先 + LLM 兜底)             │ │
│  │                    │    │                                  │ │
│  │ • TraceAnalyzer    │    │ • 规则匹配(7+ 种预定义模式)      │ │
│  │   失败聚类          │    │ • 未命中 → LLM Proposer          │ │
│  │ • TaskSuccessRate  │    │ • 生成候选 HarnessEdit           │ │
│  │   采集              │    │ • simhash 去重                   │ │
│  └────────────────────┘    └──────────────┬───────────────────┘ │
│            │                              │                      │
│            │                              ▼                      │
│            │              ┌──────────────────────────────────┐  │
│            │              │ harness_edits 表(SQLite)         │  │
│            │              │ • Candidate(等待验证)            │  │
│            │              │ • Active(生效中,注入 prompt)    │  │
│            │              │ • Retired(不再使用)              │  │
│            │              │ • success_rate 学习环            │  │
│            │              └──────────────┬───────────────────┘  │
│            │                             │                       │
│            ▼                             ▼                       │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │              两重门控验证(每 turn 执行)                   │   │
│  │                                                          │   │
│  │  Gate 1: Validity(基础设施有效性)                       │   │
│  │    • 排除网络/沙箱噪声                                    │   │
│  │    • pathology 必须在窗口内出现                           │   │
│  │                                                          │   │
│  │  Gate 2: Significance(统计显著性)                       │   │
│  │    • TaskSuccessRate vs baseline                        │   │
│  │    • z-score > 1.96 且 rate > 0.7 → 晋升 Active         │   │
│  │    • z-score < -1.96 → Retired                          │   │
│  └──────────────────────────────────────────────────────────┘   │
│                              │                                   │
│                              ▼                                   │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │        SystemPromptSplit.dynamic_sections 全量注入       │   │
│  │  • Active edits → 注入到 dynamic_sections(每 turn)      │   │
│  │  • 最多 10 条,总 token < 1.5K                            │   │
│  └──────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

### 2.2 与现有模块的关系(最大化复用)

```text
现有基础设施                    Phase 3 新增
───────────────                ──────────────
TraceAnalyzer ──────────────► WeaknessMiner(复用 cluster_failures)
DecisionLog   ──────────────► harness_edits 表(共用 SQLite 连接)
                               success_rate 学习环(复用算法)
                               simhash 去重(复用算法)
NOTEBOOK      ──────────────► 不改动(MVP lessons 段独立运行)
SystemPromptSplit ──────────► 全量注入 dynamic_sections
api_client.stream ──────────► LLM Proposer(复用调用入口)
record_turn_failed ────────► TaskSuccessRate 采集点
```

### 2.3 数据流

```text
turn N 结束
    │
    ▼
[record_turn_failed / record_turn_success]
    │
    ▼
[TraceAnalyzer 追加 TraceRecord(含 task_success: bool)]
    │
    ▼ (每 10 turn 触发,5s 超时)
[Stage 1: WeaknessMiner]
    │  • 聚类失败(复用 cluster_failures)
    │  • 过滤 occurrence_count < 2
    │  • 提取 pathology + sample_errors
    ▼
[Stage 2: Mixed Proposer]
    │  • 规则匹配:7+ 种预定义模式 → 直接生成 edit
    │  • 未命中 → LLM Proposer(高 effort,strict JSON)
    │  • simhash 去重(对比 Active + Retired)
    ▼
[harness_edits 表:存为 Candidate]
    │
    ▼ (下一轮任务执行时,每 turn 验证)
[两重门控验证]
    │  • Validity:基础设施噪声过滤 + pathology 出现确认
    │  • Significance:TaskSuccessRate z-test
    │
    ├── 通过 ──► 晋升 Active,注入 dynamic_sections
    │              │
    │              ▼
    │          [success_rate 持续学习]
    │              │
    │              ├── rate > 0.7 ──► 保持 Active
    │              ├── rate < 0.3 ──► Retired
    │              └── 中间值 ──► 继续观察
    │
    └── 未通过 ──► Retired(保留供学习)
```

---

## 三、可编辑表面定义

### 3.1 Harness Surface 分层(HASE 论文)

| 层级 | 表面 | 可编辑性 | 验证信号 | Phase 3 是否实现 |
|------|------|---------|---------|----------------|
| **L1: Guidance** | system_prompt dynamic_sections | 自由编辑 | TaskSuccessRate | ✅ 实现 |
| **L2: Runtime** | compact 阈值 / nudge 间隔 | 保守编辑 | 效率指标 | ❌ Phase 4 |
| **L3: Evaluation** | 工具定义 description | 锚定真实反馈 | 工具调用成功率 | ❌ Phase 4 |

**Phase 3 只实现 L1**:编辑 `dynamic_sections`,内容每 turn 注入到 system_prompt。

### 3.2 HarnessEdit 数据结构

```rust
/// 持久化的 harness edit,对应 dynamic_sections 中的一个可编辑段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessEdit {
    /// 唯一标识,格式:`edit-{timestamp}-{short_hash}`
    pub id: String,
    /// pathology 签名(来自 WeaknessMiner,用于质量多样性分桶)
    pub pathology: String,
    /// edit 内容(注入到 dynamic_sections 的文本)
    pub content: String,
    /// 状态(3 状态机)
    pub status: EditStatus,
    /// 来源:规则式 or LLM
    pub source: EditSource,
    /// 统计:验证次数
    pub verify_count: u32,
    /// 统计:成功次数(success_rate = success_count / verify_count)
    pub success_count: u32,
    /// 创建时间(ms since epoch)
    pub created_at: i64,
    /// 最后验证时间
    pub last_verified_at: Option<i64>,
    /// 提议来源的推理(规则式为模式名,LLM 为 reasoning)
    pub proposer_reasoning: String,
    /// simhash(用于去重,复用 decision_log::compute_simhash)
    pub similarity_hash: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EditStatus {
    /// 候选:刚提议,等待验证
    Candidate,
    /// 生效中:通过门控,正在注入 dynamic_sections
    Active,
    /// 已退役:未通过门控 或 success_rate 衰减(统一表示"不再使用")
    Retired,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EditSource {
    /// 规则式匹配(预定义模式)
    RulePattern,
    /// LLM 生成
    LlmProposer,
}
```

### 3.3 容量控制

- Active edits 最多 **10 条**(避免 dynamic_sections 膨胀)
- Candidate edits 最多 **20 条**
- Retired edits 最多 **50 条**(LRU 淘汰,保留供学习)
- 单条 content 最大 **500 chars**
- 全量注入总 token 上限 **1.5K**(10 × 500 chars ≈ 1500 tokens)

---

## 四、Stage 1: Weakness Mining(确定性代码)

### 4.1 数据源扩展

复用 `TraceAnalyzer` 的 `TraceRecord`,新增单一外部信号字段:

```rust
// trace_analyzer.rs 扩展
pub struct TraceRecord {
    pub turn_id: String,
    pub latency_ms: u64,
    pub tool_calls: u32,
    pub compact_triggered: bool,
    pub failure_kind: Option<String>,
    pub error_message: Option<String>,
    /// Phase 3 新增:任务是否成功(单一外部信号)
    pub task_success: bool,
}
```

**为什么只用 TaskSuccessRate**:turn 成功 = 编译通过 + 测试通过 + 工具调用成功。TaskSuccessRate 是其他 3 种信号的上位概念,已经包含了它们的信息,采集复杂度最低(只需 `run_turn` 返回值)。

### 4.2 WeaknessMiner(无状态函数)

```rust
/// 提取 weakness signals(无状态函数)
pub fn mine_weaknesses(
    analyzer: &TraceAnalyzer,
    lookback_turns: usize,
    min_occurrences: usize,
) -> Vec<WeaknessSignal> {
    // 1. 取最近 lookback_turns 条 TraceRecord
    let window = analyzer.recent_window(lookback_turns);

    // 2. 复用 cluster_failures(确定性分桶)
    let clusters = analyzer.cluster_failures();

    // 3. 过滤 occurrence_count < min_occurrences
    clusters.into_iter()
        .filter(|c| c.count as usize >= min_occurrences)
        .map(|c| WeaknessSignal {
            pathology: c.label,
            sample_errors: c.sample_errors,
            occurrence_count: c.count,
            related_turns: extract_related_turns(&window, &c.label),
        })
        .collect()
}

pub struct WeaknessSignal {
    /// pathology 签名(失败聚类 label)
    pub pathology: String,
    /// 样本错误消息(最多 5 条)
    pub sample_errors: Vec<String>,
    /// 窗口内出现次数
    pub occurrence_count: u32,
    /// 关联的 turn_id 列表
    pub related_turns: Vec<String>,
}
```

### 4.3 TaskSuccessRate 采集点

```rust
// conversation.rs 中扩展 record_turn_failed / record_turn_success
fn record_turn(&mut self, turn_id: &str, success: bool, failure_kind: Option<&str>) {
    if let Some(analyzer) = &self.trace_analyzer {
        let mut record = TraceRecord::new(turn_id, self.last_turn_latency_ms, self.last_turn_tool_calls)
            .with_compact_triggered(self.last_turn_compact_triggered);

        if let Some(kind) = failure_kind {
            record = record.with_failure(kind, &self.last_turn_error_message);
        }

        // Phase 3: 采集 TaskSuccessRate
        record.task_success = success;

        analyzer.lock().unwrap().add_record(record);
    }
}
```

---

## 五、Stage 2: Mixed Proposer(规则优先 + LLM 兜底)

### 5.1 混合策略架构

```rust
/// 无状态函数:混合 Proposer
pub async fn propose_edits(
    weaknesses: &[WeaknessSignal],
    existing_edits: &[HarnessEdit],
    api_client: &dyn RuntimeClient,
    config: &EvolutionConfig,
) -> Result<Vec<HarnessEdit>, ProposerError> {
    let mut proposals = Vec::new();

    // Phase A:规则式匹配(覆盖 80% 常见错误)
    for weakness in weaknesses {
        if let Some(edit) = rule_based_propose(weakness) {
            proposals.push(edit);
        }
    }

    // Phase B:LLM 兜底(处理未命中规则的 pathology)
    let unmatched: Vec<_> = weaknesses.iter()
        .filter(|w| !proposals.iter().any(|p| p.pathology == w.pathology))
        .collect();

    if !unmatched.is_empty() {
        let llm_proposals = llm_propose(&unmatched, existing_edits, api_client, config).await?;
        proposals.extend(llm_proposals);
    }

    // Phase C:simhash 去重(对比 existing_edits)
    let existing_hashes: std::collections::HashSet<i64> = existing_edits.iter()
        .map(|e| e.similarity_hash)
        .collect();
    proposals.retain(|p| !existing_hashes.contains(&p.similarity_hash));

    Ok(proposals)
}
```

### 5.2 规则式匹配(预定义模式库)

```rust
/// 预定义错误模式 → HarnessEdit 映射
const RULE_PATTERNS: &[(&str, &str, &str)] = &[
    // (pathology_keyword, edit_content, reasoning)
    (
        "old_string not found",
        "When Edit tool fails with 'old_string not found', first run Grep to locate the exact current text before retrying. Common causes: whitespace differences, partial matches, stale memory.",
        "Rule: edit_old_string_not_found — force Grep before Edit retry"
    ),
    (
        "cannot find value",
        "When Rust compile fails with 'cannot find value', check: (1) variable scope, (2) import statements, (3) typo in identifier. Use Grep to find the declaration.",
        "Rule: rust_cannot_find_value — systematic scope/import/typo check"
    ),
    (
        "unresolved import",
        "When Rust reports 'unresolved import', verify: (1) module path exists, (2) crate is in Cargo.toml, (3) use crate:: vs use :: for external crates.",
        "Rule: rust_unresolved_import — verify module path and Cargo.toml"
    ),
    (
        "connection refused",
        "When encountering 'connection refused' or 'ECONNREFUSED', before retrying: (1) check if service is running, (2) verify port number, (3) check firewall rules. Do not blindly retry.",
        "Rule: network_connection_refused — diagnose before retry"
    ),
    (
        "permission denied",
        "When 'permission denied' occurs, check: (1) file permissions (ls -la), (2) process user, (3) parent directory write access. Use chmod only if appropriate.",
        "Rule: fs_permission_denied — check permissions before write"
    ),
    (
        "no such file or directory",
        "When 'no such file or directory' occurs, verify path with LS or Glob before assuming the file exists. Common cause: relative vs absolute path confusion.",
        "Rule: fs_not_found — verify path with LS/Glob"
    ),
    (
        "test result: FAILED",
        "When tests fail, read the full failure output before modifying code. Identify: (1) which test failed, (2) assertion vs panic, (3) expected vs actual. Do not guess the fix.",
        "Rule: test_failure — analyze before fixing"
    ),
];

fn rule_based_propose(weakness: &WeaknessSignal) -> Option<HarnessEdit> {
    for (keyword, content, reasoning) in RULE_PATTERNS {
        // 检查 pathology 或 sample_errors 是否包含 keyword
        let matched = weakness.pathology.to_lowercase().contains(keyword)
            || weakness.sample_errors.iter()
                .any(|e| e.to_lowercase().contains(keyword));

        if matched {
            let simhash_text = format!("{} {}", weakness.pathology, content);
            return Some(HarnessEdit {
                id: generate_edit_id(),
                pathology: weakness.pathology.clone(),
                content: content.to_string(),
                status: EditStatus::Candidate,
                source: EditSource::RulePattern,
                verify_count: 0,
                success_count: 0,
                created_at: current_timestamp_ms(),
                last_verified_at: None,
                proposer_reasoning: reasoning.to_string(),
                similarity_hash: compute_simhash(&simhash_text) as i64,
            });
        }
    }
    None
}
```

### 5.3 LLM Proposer(兜底)

```rust
async fn llm_propose(
    weaknesses: &[&WeaknessSignal],
    existing_edits: &[HarnessEdit],
    api_client: &dyn RuntimeClient,
    config: &EvolutionConfig,
) -> Result<Vec<HarnessEdit>, ProposerError> {
    let prompt = build_proposer_prompt(weaknesses, existing_edits, config.max_proposals);

    let request = ApiRequest {
        model: config.proposer_model.clone(),
        max_tokens: 2000,
        messages: vec![ConversationMessage::user(prompt)],
        system: SystemPromptSplit::from_sections(vec![
            "You are a harness evolution proposer. Output strict JSON only.".to_string()
        ]),
        tools: None,
        tool_choice: None,
        stream: false,
        reasoning_effort: Some("high".to_string()),
        ..Default::default()
    };

    let response = api_client.stream(request).await?;
    let parsed: ProposerOutput = serde_json::from_str(&response.text())
        .map_err(|e| ProposerError::InvalidJson(e.to_string()))?;

    // 校验 + 转 HarnessEdit
    parsed.proposals.into_iter()
        .filter(|p| p.content.len() <= 500 && !p.content.is_empty())
        .map(|p| {
            let simhash_text = format!("{} {}", p.pathology, p.content);
            HarnessEdit {
                id: generate_edit_id(),
                pathology: p.pathology,
                content: p.content,
                status: EditStatus::Candidate,
                source: EditSource::LlmProposer,
                verify_count: 0,
                success_count: 0,
                created_at: current_timestamp_ms(),
                last_verified_at: None,
                proposer_reasoning: p.rationale,
                similarity_hash: compute_simhash(&simhash_text) as i64,
            }
        })
        .collect::<Vec<_>>()
        .into_ok()
}
```

### 5.4 LLM Proposer Prompt 模板

```text
You are a harness evolution proposer for the Claw AI coding agent.

Your task: analyze UNMATCHED failure patterns (not covered by predefined rules)
and propose MINIMAL, TARGETED harness edits.

## Current Active Edits (do not duplicate)
{existing_edits_summary}

## Unmatched Failure Patterns
{weaknesses_json}

## Rules (CRITICAL — violations will be rejected)
1. Propose ONLY for pathology with occurrence_count >= 2
2. Content MUST be a concrete, testable instruction (max 500 chars)
3. Do NOT propose generic advice like "be more careful"
4. Do NOT propose more than {max_proposals} edits
5. Each edit MUST reference a specific failure pattern

## Output Format (strict JSON)
{
  "reasoning": "Brief analysis",
  "proposals": [
    {
      "pathology": "specific_failure_signature",
      "content": "Concrete actionable instruction",
      "rationale": "Why this would help"
    }
  ]
}

Generate proposals now.
```

---

## 六、持久化:独立 SQLite 表(共用 decision_log.db)

### 6.1 Schema 设计

```sql
-- 在 decision_log.db 中新增 harness_edits 表(共用连接,独立 schema)
CREATE TABLE IF NOT EXISTS harness_edits (
    id TEXT PRIMARY KEY,
    pathology TEXT NOT NULL,
    content TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('Candidate', 'Active', 'Retired')),
    source TEXT NOT NULL CHECK(source IN ('RulePattern', 'LlmProposer')),
    verify_count INTEGER DEFAULT 0,
    success_count INTEGER DEFAULT 0,
    created_at INTEGER NOT NULL,
    last_verified_at INTEGER,
    proposer_reasoning TEXT,
    similarity_hash INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_harness_edits_status ON harness_edits(status);
CREATE INDEX IF NOT EXISTS idx_harness_edits_pathology ON harness_edits(pathology);
CREATE INDEX IF NOT EXISTS idx_harness_edits_simhash ON harness_edits(similarity_hash);

-- FTS5 全文搜索(复用 DecisionLog 的模式)
CREATE VIRTUAL TABLE IF NOT EXISTS harness_edits_fts USING fts5(
    pathology, content,
    content='harness_edits', content_rowid='rowid'
);

-- 同步触发器(与 decisions_fts 一致模式)
CREATE TRIGGER IF NOT EXISTS harness_edits_ai AFTER INSERT ON harness_edits BEGIN
    INSERT INTO harness_edits_fts(rowid, pathology, content)
    VALUES (new.rowid, new.pathology, new.content);
END;

CREATE TRIGGER IF NOT EXISTS harness_edits_ad AFTER DELETE ON harness_edits BEGIN
    INSERT INTO harness_edits_fts(harness_edits_fts, rowid, pathology, content)
    VALUES ('delete', old.rowid, old.pathology, old.content);
END;

CREATE TRIGGER IF NOT EXISTS harness_edits_au AFTER UPDATE ON harness_edits BEGIN
    INSERT INTO harness_edits_fts(harness_edits_fts, rowid, pathology, content)
    VALUES ('delete', old.rowid, old.pathology, old.content);
    INSERT INTO harness_edits_fts(rowid, pathology, content)
    VALUES (new.rowid, new.pathology, new.content);
END;
```

### 6.2 HarnessArchive 模块(复用 SQLite 连接)

```rust
/// 新增文件: rust/crates/runtime/src/harness_evolution/archive.rs
///
/// HarnessEdit 持久化层,共用 decision_log.db 的 SQLite 连接,
/// 但拥有独立的 schema(harness_edits 表)。
pub struct HarnessArchive {
    conn: Mutex<Connection>,
}

impl HarnessArchive {
    /// 打开 archive(共用 decision_log.db 路径)
    pub fn open(root: &Path) -> Result<Self, ArchiveError> {
        let db_path = root.join(".claw").join("decision_log.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        migrate_harness_schema(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// 新增 Candidate edit
    pub fn add_candidate(&self, edit: HarnessEdit) -> Result<(), ArchiveError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO harness_edits (id, pathology, content, status, source,
             verify_count, success_count, created_at, last_verified_at,
             proposer_reasoning, similarity_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                edit.id, edit.pathology, edit.content,
                edit.status.as_db_str(), edit.source.as_db_str(),
                edit.verify_count, edit.success_count,
                edit.created_at, edit.last_verified_at,
                edit.proposer_reasoning, edit.similarity_hash
            ],
        )?;
        Ok(())
    }

    /// 查询所有 Active edits(用于注入 dynamic_sections)
    pub fn active_edits(&self) -> Result<Vec<HarnessEdit>, ArchiveError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM harness_edits WHERE status = 'Active'
             ORDER BY success_count DESC LIMIT 10"
        )?;
        // ... map rows to HarnessEdit
    }

    /// 查询所有 Candidate edits(用于验证)
    pub fn candidate_edits(&self) -> Result<Vec<HarnessEdit>, ArchiveError> {
        // ... WHERE status = 'Candidate'
    }

    /// 更新 edit 状态 + 统计
    pub fn update_status_and_stats(
        &self,
        edit_id: &str,
        new_status: EditStatus,
        verify_count: u32,
        success_count: u32,
    ) -> Result<(), ArchiveError> {
        // ... UPDATE harness_edits SET status = ?, verify_count = ?, ...
    }

    /// 一键回滚所有 Active edits
    pub fn rollback_all(&self) -> Result<u32, ArchiveError> {
        let conn = self.conn.lock().unwrap();
        let count = conn.execute(
            "UPDATE harness_edits SET status = 'Retired' WHERE status = 'Active'",
            []
        )?;
        Ok(count as u32)
    }

    /// 回滚单个 edit
    pub fn rollback(&self, edit_id: &str) -> Result<(), ArchiveError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE harness_edits SET status = 'Retired' WHERE id = ?1 AND status = 'Active'",
            params![edit_id]
        )?;
        Ok(())
    }

    /// simhash 去重查询(汉明距离 <= 3 视为重复)
    pub fn find_similar(&self, simhash: i64, threshold: u32) -> Result<Vec<HarnessEdit>, ArchiveError> {
        // 复用 decision_log::hamming_distance
        // 查询所有 Active + Retired,计算汉明距离
        // ... WHERE hamming_distance(similarity_hash, ?) <= threshold
    }

    /// 统计信息(用于 CLI)
    pub fn stats(&self) -> Result<ArchiveStats, ArchiveError> {
        // SELECT status, COUNT(*) GROUP BY status
        // ... 计算平均 success_rate 等
    }
}
```

### 6.3 success_rate 学习环(复用 DecisionLog 算法)

```rust
/// 复用 decision_log.rs:602 的 verify_decision 算法
pub fn update_edit_success_rate(
    archive: &HarnessArchive,
    edit_id: &str,
    verification: DecisionVerification,
) -> Result<(), ArchiveError> {
    let mut edit = archive.get_edit(edit_id)?;
    let signal = verification.signal();

    let old_rate = if edit.verify_count > 0 {
        edit.success_count as f64 / edit.verify_count as f64
    } else {
        0.0
    };

    let new_rate = (old_rate * edit.verify_count as f64 + signal)
        / (edit.verify_count as f64 + 1.0);

    edit.verify_count += 1;
    if verification.updates_stats() {
        edit.success_count = (new_rate * edit.verify_count as f64).round() as u32;
    }
    edit.last_verified_at = Some(current_timestamp_ms());

    // 回滚检查:verify_count >= 5 且 rate < 0.3 → Retired
    if edit.verify_count >= 5 && new_rate < 0.3 && edit.status == EditStatus::Active {
        edit.status = EditStatus::Retired;
    }

    archive.update_status_and_stats(
        edit_id, edit.status, edit.verify_count, edit.success_count
    )?;
    Ok(())
}
```

---

## 七、两重门控验证(确定性代码)

### 7.1 两重门控机制(基于 GSME,删除 Activation Gate)

```rust
/// 新增文件: rust/crates/runtime/src/harness_evolution/validator.rs

pub struct EvolutionConfig {
    /// 验证窗口:候选 edit 需要观察 N 个 turn
    pub validation_window: usize,
    /// 显著性测试阈值(alpha)
    pub significance_alpha: f64,
    /// success_rate 晋升阈值
    pub promote_threshold: f64,
    /// success_rate 回滚阈值
    pub rollback_threshold: f64,
    /// evolution 触发间隔(每 N turn)
    pub evolution_interval: usize,
    /// LLM Proposer 超时(秒)
    pub proposer_timeout_secs: u64,
    /// proposer 模型
    pub proposer_model: String,
    /// 每次最大提议数
    pub max_proposals: usize,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            validation_window: 10,
            significance_alpha: 0.05,  // z > 1.96
            promote_threshold: 0.7,
            rollback_threshold: 0.3,
            evolution_interval: 10,
            proposer_timeout_secs: 5,
            proposer_model: "claude-sonnet-4-5".to_string(),
            max_proposals: 3,
        }
    }
}

/// 验证结果
pub enum ValidationOutcome {
    /// 晋升为 Active
    Promoted,
    /// 继续保持 Candidate,等待更多数据
    StillCandidate(String),
    /// 退役,标记为 Retired
    Retired(String),
}

/// 两重门控验证
pub fn validate_candidate(
    candidate: &HarnessEdit,
    trace_window: &[TraceRecord],
    baseline_rate: f64,
    config: &EvolutionConfig,
) -> ValidationOutcome {
    // Gate 1: Validity(基础设施有效性)
    if let Err(reason) = validity_gate(candidate, trace_window) {
        return ValidationOutcome::Retired(reason);
    }

    // Gate 2: Significance(统计显著性)
    match significance_gate(candidate, trace_window, baseline_rate, config) {
        SignificanceResult::Promote => ValidationOutcome::Promoted,
        SignificanceResult::Keep => ValidationOutcome::StillCandidate("insufficient data".into()),
        SignificanceResult::Reject => ValidationOutcome::Retired("significant degradation".into()),
    }
}
```

### 7.2 Gate 1: Validity(基础设施有效性)

**目的**:排除基础设施噪声导致的假阳性。

```rust
fn validity_gate(
    candidate: &HarnessEdit,
    trace_window: &[TraceRecord],
) -> Result<(), String> {
    // 1. 检查窗口内基础设施失败占比
    let infra_failures = trace_window.iter()
        .filter(|t| matches!(t.failure_kind.as_deref(),
            Some("network_timeout") | Some("sandbox_crash") | Some("verifier_timeout")))
        .count();

    if !trace_window.is_empty() && infra_failures * 3 > trace_window.len() {
        return Err("infrastructure failures dominate window, results unreliable".into());
    }

    // 2. 检查 candidate 的 pathology 是否在窗口内出现
    let pathology_occurrences = trace_window.iter()
        .filter(|t| t.failure_kind.as_deref() == Some(&candidate.pathology))
        .count();

    if pathology_occurrences == 0 {
        return Err("pathology did not occur in validation window".into());
    }

    Ok(())
}
```

### 7.3 Gate 2: Significance(统计显著性)

**目的**:paired comparison,确认 edit 带来的提升是统计显著的。

```rust
enum SignificanceResult {
    Promote,
    Keep,
    Reject,
}

fn significance_gate(
    candidate: &HarnessEdit,
    trace_window: &[TraceRecord],
    baseline_rate: f64,
    config: &EvolutionConfig,
) -> SignificanceResult {
    // 1. 计算窗口内 TaskSuccessRate
    let window_success_rate = compute_task_success_rate(trace_window);

    // 2. 与 baseline 对比(z-test,简化版)
    let n = trace_window.len() as f64;
    if n < 3.0 {
        return SignificanceResult::Keep;  // 样本不足
    }

    let diff = window_success_rate - baseline_rate;
    let std_error = (baseline_rate * (1.0 - baseline_rate) / n).sqrt();
    let z_score = if std_error > 0.0 { diff / std_error } else { 0.0 };

    // 3. 决策(alpha = 0.05 → z_threshold = 1.96)
    let threshold = 1.96;  // 对应 alpha = 0.05

    if z_score > threshold && window_success_rate > config.promote_threshold {
        return SignificanceResult::Promote;
    }

    if z_score < -threshold {
        return SignificanceResult::Reject;
    }

    SignificanceResult::Keep
}

fn compute_task_success_rate(trace_window: &[TraceRecord]) -> f64 {
    if trace_window.is_empty() {
        return 0.0;
    }
    let successes = trace_window.iter().filter(|t| t.task_success).count();
    successes as f64 / trace_window.len() as f64
}
```

---

## 八、模块组织(3 文件)

### 8.1 文件结构

```text
rust/crates/runtime/src/
├── harness_evolution/           ← 新增模块(3 文件)
│   ├── mod.rs                   ← 模块导出 + evolve() 主入口
│   ├── types.rs                 ← HarnessEdit, EditStatus, EditSource, EvolutionConfig
│   └── archive.rs               ← HarnessArchive(SQLite 持久化)+ 验证逻辑内联
├── lib.rs                       ← 新增 pub mod harness_evolution;
└── conversation.rs              ← 集成点(见 8.3)
```

### 8.2 主入口:无状态函数

```rust
/// 新增文件: rust/crates/runtime/src/harness_evolution/mod.rs

pub mod types;
pub mod archive;

pub use types::*;
pub use archive::HarnessArchive;

use crate::trace_analyzer::TraceAnalyzer;
use crate::decision_log::{compute_simhash, hamming_distance, DecisionVerification};

/// Evolution 主入口(无状态函数)
///
/// 在 conversation.rs 的 turn 结束后调用,同步执行(限频 + 超时保护)。
pub async fn evolve(
    trace: &TraceAnalyzer,
    archive: &HarnessArchive,
    api_client: &dyn RuntimeClient,
    config: &EvolutionConfig,
) -> Result<EvolutionReport, EvolutionError> {
    // Stage 1: Weakness Mining
    let weaknesses = mine_weaknesses(trace, config.validation_window, 2);
    if weaknesses.is_empty() {
        // 仍然验证已有 candidates
        validate_all_candidates(trace, archive, config)?;
        return Ok(EvolutionReport::no_weaknesses());
    }

    // Stage 2: Mixed Proposer(规则优先 + LLM 兜底)
    let existing = archive.active_edits()?;
    let proposals = propose_edits(&weaknesses, &existing, api_client, config).await?;
    for proposal in proposals {
        archive.add_candidate(proposal)?;
    }

    // Stage 3: 验证所有 Candidate edits
    validate_all_candidates(trace, archive, config)?;

    Ok(EvolutionReport::from_archive(archive)?)
}

/// 验证所有 Candidate edits(每 turn 调用)
fn validate_all_candidates(
    trace: &TraceAnalyzer,
    archive: &HarnessArchive,
    config: &EvolutionConfig,
) -> Result<(), EvolutionError> {
    let trace_window = trace.recent_window(config.validation_window);
    let baseline_rate = compute_baseline_rate(trace);
    let candidates = archive.candidate_edits()?;

    for candidate in candidates {
        let outcome = validate_candidate(&candidate, &trace_window, baseline_rate, config);
        match outcome {
            ValidationOutcome::Promoted => {
                archive.update_status(&candidate.id, EditStatus::Active)?;
            }
            ValidationOutcome::StillCandidate(_) => {
                // 不改状态,继续观察
            }
            ValidationOutcome::Retired(reason) => {
                archive.update_status(&candidate.id, EditStatus::Retired)?;
                archive.set_retire_reason(&candidate.id, &reason)?;
            }
        }
    }
    Ok(())
}

/// 生成注入到 dynamic_sections 的文本(全量注入)
pub fn render_for_injection(archive: &HarnessArchive) -> Result<Vec<String>, EvolutionError> {
    let active = archive.active_edits()?;
    Ok(active.into_iter().map(|e| e.content).collect())
}

#[derive(Debug)]
pub struct EvolutionReport {
    pub weaknesses_count: usize,
    pub proposals_count: usize,
    pub promoted_count: usize,
    pub retired_count: usize,
}
```

### 8.3 conversation.rs 集成点

**字段新增**(L290-419 附近):

```rust
pub struct ConversationRuntime<C, T> {
    // ... 现有字段 ...
    turns_since_last_nudge: usize,
    turns_since_last_distill: usize,  // MVP 已规划
    /// Phase 3: 自进化 turn 计数器
    turns_since_last_evolution: usize,
    /// Phase 3: HarnessArchive(Option,可禁用)
    harness_archive: Option<HarnessArchive>,
}
```

**触发点**(L1855 后,nudge 块结束,`Ok(summary)` 前):

```rust
// 现有 nudge 逻辑...
// self.turns_since_last_nudge = 0;
// }  ← nudge 块结束

// Phase 3: 自进化触发(同步限频 + 超时保护)
if let Some(archive) = &self.harness_archive {
    if let Some(trace_analyzer) = &self.trace_analyzer {
        self.turns_since_last_evolution += 1;
        let config = EvolutionConfig::default();

        if self.turns_since_last_evolution >= config.evolution_interval {
            let trace = trace_analyzer.lock().unwrap().clone();

            // 同步触发,带超时保护(规则式路径零延迟,LLM 路径最多 5s)
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(config.proposer_timeout_secs),
                harness_evolution::evolve(&trace, archive, &*self.api_client, &config)
            ).await;

            match result {
                Ok(Ok(report)) => {
                    tracing::debug!(
                        "harness evolution: {} weaknesses, {} proposals, {} promoted, {} retired",
                        report.weaknesses_count, report.proposals_count,
                        report.promoted_count, report.retired_count
                    );
                    self.turns_since_last_evolution = 0;
                }
                Ok(Err(e)) => {
                    tracing::warn!("harness evolution error: {e}");
                }
                Err(_) => {
                    tracing::warn!("harness evolution timeout ({}s)", config.proposer_timeout_secs);
                }
            }
        }
    }
}

Ok(summary)  // ← 现有代码
```

**注入点**(L1068-1074 附近,NOTEBOOK 注入后):

```rust
// 现有 NOTEBOOK 注入...
// system_split.dynamic_sections.push(notebook_prompt);

// Phase 3: 注入生效中的 harness edits(全量注入)
if let Some(archive) = &self.harness_archive {
    if let Ok(edit_sections) = harness_evolution::render_for_injection(archive) {
        for section in edit_sections {
            system_split.dynamic_sections.push(section);
        }
    }
}
```

---

## 九、防 misevolution 机制

### 9.1 四重防护

| 层级 | 机制 | 实现 | 论文依据 |
|------|------|------|---------|
| 1 | Proposing/Crediting 分离 | LLM 只提议,确定性代码归因 | GSME |
| 2 | 外部信号门控 | TaskSuccessRate(单一信号,禁止纯 LLM 自评) | Misevolution |
| 3 | 两重门控 | Validity + Significance(z-test, alpha=0.05) | GSME |
| 4 | 可回滚 | SQLite 持久化 + 一键回滚 + Retired 状态机 | 工程实践 |

### 9.2 禁止事项

1. **禁止 LLM 自评生效**:LLM 不能决定自己的 edit 是否晋升
2. **禁止无信号晋升**:没有 TaskSuccessRate 的 edit 永远停留 Candidate
3. **禁止全量注入超限**:最多 10 条 Active edits,总 token < 1.5K
4. **禁止跨表面编辑**:Phase 3 只能编辑 dynamic_sections
5. **禁止无 pathology 提议**:每个 edit 必须关联具体失败模式

### 9.3 回滚机制

```rust
// CLI 命令:claw harness rollback --all 或 claw harness rollback --id <edit_id>

// 紧急回滚所有 Active edits
pub fn rollback_all(&self) -> Result<u32, ArchiveError> {
    let conn = self.conn.lock().unwrap();
    let count = conn.execute(
        "UPDATE harness_edits SET status = 'Retired' WHERE status = 'Active'",
        []
    )?;
    Ok(count as u32)
}
```

---

## 十、CLI 集成

### 10.1 新增命令

```bash
# 查看所有 edits(按状态分组)
claw harness list [--status <Candidate|Active|Retired>]

# 查看统计
claw harness stats

# 回滚
claw harness rollback --all
claw harness rollback --id <edit_id>

# 手动触发 evolution(调试用)
claw harness evolve --dry-run
```

### 10.2 输出示例

```text
$ claw harness list

Active Edits (3):
  edit-1784829254-a1b2 | pathology: edit_old_string_not_found | rate: 0.85 | verify: 12 | source: RulePattern
  edit-1784830000-c3d4 | pathology: rust_unresolved_import    | rate: 0.78 | verify: 8  | source: RulePattern
  edit-1784831000-e5f6 | pathology: custom_api_timeout        | rate: 0.90 | verify: 5  | source: LlmProposer

Candidate Edits (2):
  edit-1784840000-g7h8 | pathology: fs_permission_denied     | verify: 2 | awaiting more data
  edit-1784841000-i9j0 | pathology: test_compile_error       | verify: 1 | awaiting more data

Retired Edits (1):
  edit-1784820000-k1l2 | pathology: generic_error            | reason: significant degradation

$ claw harness stats

Evolution Stats:
  Total proposals: 15
  Active: 3 (20%)
  Candidate: 2 (13%)
  Retired: 10 (67%)
  Average success_rate (Active): 0.84
  Rule-sourced: 10 (67%)
  LLM-sourced: 5 (33%)
  LLM calls saved by rule matching: ~70%
```

---

## 十一、实施任务拆解(12 Task,一次到位)

### Phase 3.1: 基础设施(4 Task)

| Task | 内容 | 文件 | 依赖 |
|------|------|------|------|
| T1 | 创建 harness_evolution 模块 + 类型定义 | mod.rs, types.rs, lib.rs | 无 |
| T2 | 扩展 TraceRecord 新增 task_success 字段 | trace_analyzer.rs | T1 |
| T3 | 扩展 record_turn 采集 TaskSuccessRate | conversation.rs | T2 |
| T4 | 实现 HarnessArchive(SQLite schema + CRUD) | archive.rs | T1 |

### Phase 3.2: 三阶段循环(5 Task)

| Task | 内容 | 文件 | 依赖 |
|------|------|------|------|
| T5 | 实现 mine_weaknesses(复用 cluster_failures) | mod.rs | T2 |
| T6 | 实现规则式 Proposer(RULE_PATTERNS) | mod.rs | T5 |
| T7 | 实现 LLM Proposer(prompt + 调用 + 解析) | mod.rs | T6 |
| T8 | 实现混合 Proposer(规则优先 + LLM 兜底 + simhash 去重) | mod.rs | T6, T7 |
| T9 | 实现两重门控验证(Validity + Significance) | mod.rs | T5 |

### Phase 3.3: 集成与验证(3 Task)

| Task | 内容 | 文件 | 依赖 |
|------|------|------|------|
| T10 | 实现 evolve() 主入口 + validate_all_candidates | mod.rs | T8, T9 |
| T11 | conversation.rs 集成(字段 + 触发 + 注入) | conversation.rs | T10 |
| T12 | CLI 集成(claw harness list/stats/rollback) | commands_handler.rs, doctor.rs | T4 |

### 任务依赖图

```text
T1 ──► T4 ──────────────────────────┐
  │                                  │
  ├──► T2 ──► T3                     │
  │      │                           │
  │      └──► T5 ──► T6 ──► T7 ──► T8│
  │              │              │    │
  │              └──► T9 ◄──────┤    │
  │                  │          │    │
  │                  ▼          ▼    ▼
  └────────────► T10 ◄────────────────┘
                   │
                   ├──► T11
                   └──► T12
```

---

## 十二、风险评估

### 12.1 技术风险

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| LLM Proposer 生成低质量 edit | 高 | 中 | 两重门控 + 规则优先(70% 不调 LLM)|
| TaskSuccessRate 信号噪声 | 中 | 高 | Validity Gate 过滤基础设施失败 |
| dynamic_sections 膨胀 | 低 | 中 | 10 条上限 + 全量注入 <1.5K tokens |
| LLM 调用超时阻塞主循环 | 中 | 高 | 5s 超时 + 规则式路径零延迟 |
| SQLite 并发竞争(与 DecisionLog 共用) | 低 | 中 | WAL 模式 + Mutex |

### 12.2 misevolution 风险

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| edit 过拟合到特定任务 | 高 | 高 | pathology 分桶(非 task 分桶)|
| success_rate 被噪声污染 | 中 | 高 | Significance Gate + 最小样本量 |
| edit 互相冲突 | 中 | 中 | simhash 去重 + 10 条上限 |
| LLM Proposer 自评通过 | 低 | 致命 | 架构保证:LLM 无法修改 archive 状态 |

### 12.3 回滚方案

1. **立即回滚**:`claw harness rollback --all`
2. **代码回滚**:移除 `harness_archive` 字段 + 注入逻辑 + 触发逻辑(3 处改动)
3. **数据清理**:删除 `.claw/decision_log.db` 中的 `harness_edits` 表(`DROP TABLE`)
4. **保留基础设施**:TraceRecord 的 task_success 字段可保留,不影响现有功能

---

## 十三、评估指标

### 13.1 离线评估

| 指标 | 采集方式 | 目标 |
|------|---------|------|
| 重复错误修复 turn 数 | 对比有/无 evolution | 降低 50%+ |
| 跨会话能力保持 | 新会话首次任务成功率 | 提升 20%+ |
| Active edits 的 success_rate | archive 统计 | > 0.7 |
| Candidate → Active 晋升率 | archive 统计 | 30-50% |
| LLM 调用节省率 | rule vs llm 比例 | > 70% |

### 13.2 在线监控

通过 `claw harness stats` 持续监控,关键告警:
- Active edits 的平均 success_rate < 0.5 → 建议回滚
- Candidate → Active 晋升率 < 10% → Proposer 质量问题
- Retired edits 中 LLM-sourced 占比 > 50% → LLM Proposer 质量差

---

## 十四、未来扩展(Phase 4+)

| Phase | 扩展内容 | 依赖 |
|-------|---------|------|
| Phase 4.1 | L2 Runtime 编辑(compact 阈值/nudge 间隔) | Phase 3 稳定运行 |
| Phase 4.2 | L3 Evaluation 编辑(工具定义 description) | Phase 4.1 |
| Phase 4.3 | 多模型协作(Proposer 用更强模型) | API 支持 |
| Phase 4.4 | 与 DecisionLog 融合(Active edit 自动晋升为 Decision) | Phase 4.2 |
| Phase 4.5 | 跨项目 harness 共享(导出/导入 edits) | Phase 4.4 |

### 与 DecisionLog 的融合(Phase 4.4)

```text
HarnessEdit (Phase 3)          DecisionRecord (现有)
    │                               │
    │ success_rate > 0.8            │ verified
    │ + verify_count > 10           │ + use_count > 5
    ▼                               ▼
    ┌─────────────────────────────────┐
    │  DecisionLog (持久化经验库)      │
    │  • 跨会话共享                    │
    │  • simhash 去重                  │
    │  • FTS5 全文搜索                 │
    │  • 语义检索                      │
    └─────────────────────────────────┘
```

---

## 十五、参考文献

1. **GSME**: "Self-Evolving Agent Harnesses via Gated Semantic Quality-Diversity", arXiv:2607.13683, 2026-07
   - Proposing/Crediting 分离(本方案核心借鉴)
   - 门控质量多样性归档
   - 7 领域 +9~15.5pp 提升

2. **HASE**: "Harness-Aware Self-Evolving", arXiv:2607.03935, 2026-07
   - Guidance/Evaluation 组件分离(本方案 L1/L2/L3 分层依据)

3. **ERL**: "Experiential Reflective Learning", ICLR 2026 MemAgents Workshop
   - Single-attempt trajectory extraction
   - Gaia2 +7.8% over ReAct

4. **ExpeL**: "LLM Agents Are Experiential Learners", arXiv:2308.10144, AAAI 2024
   - 经验提取 → insights 维护 → 推理时召回

5. **Misevolution**: arXiv:2509.26354
   - 99.3% 自我改进是有界自优化(本方案防 misevolution 设计依据)
   - 评估器必须用外部信号

6. **Self-Harness**: (量子位报道, LangChain CEO 转发)
   - Weakness Mining → Harness Proposal → Proposal Validation
   - Qwen3.5-35B +104%, GLM-5 +24%

---

## 附录 A:与原方案的差异对比

| 维度 | 原方案 | 优化后 | 理由 |
|------|--------|--------|------|
| 门控 | 三重(Validity + Activation + Significance) | 两重(Validity + Significance) | Activation 在小样本下信号弱,被 Validity 覆盖 |
| Proposer | 纯 LLM | 规则优先 + LLM 兜底 | 80% 常见错误规则覆盖,减少 70% LLM 调用 |
| 持久化 | 独立 JSON(GatedArchive 模块) | 独立 SQLite 表(共用 decision_log.db) | 复用 SQLite 事务 + FTS5 + 独立 schema |
| 外部信号 | 4 种(Compile/Test/ToolCall/Task) | 1 种(TaskSuccessRate) | TaskSuccessRate 是其他 3 种的上位概念 |
| 注入 | 检索式 top-k | 全量注入(10 条上限) | 10 条 × 500 chars ≈ 1.5K tokens,检索不划算 |
| 触发 | 异步 tokio::spawn | 同步限频(每 10 turn + 5s 超时) | 规则式零延迟,异步带来状态一致性难题 |
| 状态机 | 4 状态(Candidate/Active/Rejected/RolledBack) | 3 状态(Candidate/Active/Retired) | Rejected 和 RolledBack 无 actionable 区分价值 |
| 模块文件 | 7 文件 | 3 文件 | 内聚到 mod.rs + types.rs + archive.rs |
| 协调器 | 有状态 EvolutionCoordinator | 无状态 evolve() 函数 | 组件无状态,只需传入 trace + archive |
| Task 数 | 15 Task / 3 周 | 12 Task / 2.5 周 | 复用现有基础设施,减少新代码 |

## 附录 B:预期收益对比

| 维度 | MVP(规则式) | 原方案(LLM) | 优化后 | 差异 |
|------|-------------|------------|--------|------|
| 能力提升 | 10-15% | 35-50% | 35-50% | 相同(混合策略不损失覆盖面)|
| LLM 调用成本 | 0 | 每次 evolution 1 次 | 每次 evolution 0.3 次 | -70% |
| 实施成本 | 8 Task | 15 Task | 12 Task | -20% |
| misevolution 风险 | 极低 | 极低 | 低 | 略增(门控简化)但可接受 |
| 向后兼容 | A 级 | A 级 | A 级 | 相同 |
