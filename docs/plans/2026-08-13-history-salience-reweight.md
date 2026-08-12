# 历史检索 Salience 重加权(Phase 2)实现方案

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 Phase 1 的混合检索(词法 + 稠密 + RRF 融合)之上，增加规则式 salience 重加权层：对融合后的候选按"角色 + 内容信号词"放大重要性，提升决策点与结论性消息的召回优先级。

**Architecture:** 对应 True Memory 论文(arXiv:2605.04897)检索管线的 **Stage 9 / L3 salience reweighter**(条件于说话人画像)。CLAW 落地为零 LLM 成本的规则式函数 `salience_weight(role, content) -> f64`(乘子)，作用于 `hybrid_search` 的 RRF 融合结果之后：`final_rank = rrf_score × salience_weight`，重新降序排序。**仅作用于混合融合路径**——无 embedder 回退纯词法时行为与 `search()` 完全一致(保持 Phase 1 承诺)。

**Tech Stack:** Rust、rusqlite 0.31(bundled FTS5)、hash-embedding 测试替身。全部改动在单文件 `history_search.rs`。

---

## 背景与现状(代码事实)

- [history_search.rs](d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L274-L300) `hybrid_search`：lexical(`search()`，含 decision role rank×2.0)→ dense(`dense_search`，余弦)→ `rrf_merge`(RRF_K=60)→ top_k。返回 `rank` = RRF 融合分数(越高越相关)。
- [history_search.rs](d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L536-L562) `rrf_merge`：键 `(session_id, message_index, role)`，双列命中叠加贡献。
- 既有角色加权先例：`search()` 内 decision role rank×2.0([history_search.rs](d:/claw-code-src/rust/crates/runtime/src/history_search.rs#L164-L170))——这是词法路径内部的加权，Phase 2 的 salience 层与之**叠加**(决策点最高优先是明确意图，见设计决策 4)。
- 可复用信号词先例：[task_state.rs](d:/claw-code-src/rust/crates/runtime/src/task_state.rs#L51-L85) 的 `FINDING_KEYWORDS`/`FINDING_STRONG_MARKERS`(私有常量，Phase 2 在 history_search.rs 定义自己的信号词表，避免跨模块耦合)。

## 关键设计决策

1. **乘子而非加性**：RRF 分数量级 ~1/(60+rank)≈0.016，加性权重会被淹没；乘子符合"显著性放大"语义，与 True Memory 的 reweight 一致。
2. **role 基值**(salience 的"说话人画像"条件化，L0 engram 类比)：
   - `decision` = 1.5(决策点)
   - `assistant` = 1.2(助手陈述，多含结论)
   - `user` / `tool` = 1.0(基线)
3. **内容信号词累加**(大小写不敏感子串匹配，每命中 +0.35 / +0.25 / +0.2，总加成封顶 +1.0)：
   - 结论强标记(+0.35)：`根因是` `原因是` `确认` `已验证` `结论` `已修复` `修复了` `PASS` `FAIL` `found that` `verified` `root cause`
   - 错误信号(+0.25)：`error` `panic` `fail` `failed` `报错` `失败` `异常`
   - 决策信号(+0.2)：`decided` `decision` `决定` `方案` `alternatives`
4. **与 search() 内 decision×2.0 的关系**：接受叠加。`search()` 内部加权服务于纯词法路径；salience 层是融合路径的统一加权。总效应 decision ≈ ×3.0，符合"决策点最优先"意图，代码注释明确说明。
5. **仅混合路径生效**：`hybrid_search` 无 embedder 或 dense 为空时直接返回 lexical(`search()` 结果)，**不应用** salience —— 保持"无 embedder 行为与 Phase 1 完全一致"。salience 是融合管线的一层(对应 True Memory Stage 9)。
6. **恒开启、常量可调**：权重用 `pub const` 暴露，暂不接 config 开关(避免过度工程)。

## 文件结构

| 文件 | 职责 | 变更类型 |
|---|---|---|
| `rust/crates/runtime/src/history_search.rs` | salience 常量 + `salience_weight` 函数 + `hybrid_search` 集成 + 测试 | 修改(全部) |

---

## Task 1: `salience_weight` 规则式打分函数

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs`(常量区 + 自由函数区)

- [ ] **Step 1: 写失败测试**

在测试模块追加:

```rust
    // -----------------------------------------------------------------
    // Phase 2:salience 重加权
    // -----------------------------------------------------------------

    #[test]
    fn salience_weight_role_base_ordering() {
        // 决策点 > 助手 > 用户/工具
        let decision = salience_weight("decision", "plain text");
        let assistant = salience_weight("assistant", "plain text");
        let user = salience_weight("user", "plain text");
        let tool = salience_weight("tool", "plain text");
        assert!(decision > assistant, "decision should outrank assistant");
        assert!(assistant > user, "assistant should outrank user");
        assert_eq!(user, tool, "user and tool share the baseline");
        assert_eq!(user, 1.0, "user baseline should be 1.0");
    }

    #[test]
    fn salience_weight_content_signals_add_up() {
        // 结论强标记提升 assistant 陈述
        let with_conclusion = salience_weight("assistant", "根因是缓存失效,已修复,测试 PASS");
        let plain = salience_weight("assistant", "plain text without signals");
        assert!(
            with_conclusion > plain,
            "conclusion signals should raise salience: {} vs {}",
            with_conclusion,
            plain
        );
        // 错误信号提升 tool 结果
        let with_error = salience_weight("tool", "command failed with panic: timeout");
        let tool_plain = salience_weight("tool", "completed");
        assert!(with_error > tool_plain, "error signals should raise salience");
        // 决策信号提升 user 消息
        let with_decision = salience_weight("user", "decided to use rust toolchain");
        assert!(with_decision > 1.0, "decision signal should raise salience");
    }

    #[test]
    fn salience_weight_caps_content_bonus() {
        // 多个信号词命中,内容加成封顶 +1.0
        let mut text = String::new();
        for _ in 0..10 {
            text.push_str("根因是 confirmed verified PASS ");
        }
        let score = salience_weight("user", &text);
        assert!(
            score <= 2.0,
            "content bonus should cap at +1.0, got {}",
            score
        );
    }

    #[test]
    fn salience_weight_case_insensitive() {
        let upper = salience_weight("tool", "PANIC: VERIFIED FAIL");
        let lower = salience_weight("tool", "panic: verified fail");
        assert_eq!(upper, lower, "signals should be case-insensitive");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::salience_weight_role_base_ordering`
Expected: FAIL(编译错误：`salience_weight` 未定义)。

- [ ] **Step 3: 实现常量 + 函数**

在模块常量区(`RRF_K` 附近)新增:

```rust
// ── Phase 2:salience 重加权 ──
// 对应 True Memory(L3 salience reweighter)检索期显著性加权。
// 规则式、零 LLM 成本:按角色基值 + 内容信号词累加,乘子作用于 RRF 融合分数。

/// decision 角色 salience 基值(决策点最优先;与 search() 内 decision rank×2.0 叠加,
/// 总效应 ≈×3.0,符合"决策点最高优先"意图)。
pub const SALIENCE_ROLE_DECISION: f64 = 1.5;
/// assistant 角色 salience 基值(助手陈述多含结论)。
pub const SALIENCE_ROLE_ASSISTANT: f64 = 1.2;
/// user / tool 角色 salience 基线。
pub const SALIENCE_ROLE_BASELINE: f64 = 1.0;
/// 单次内容信号词命中的加成。
pub const SALIENCE_SIGNAL_WEIGHT: f64 = 0.35;
/// 错误信号词命中加成(低于结论强标记)。
pub const SALIENCE_SIGNAL_WEIGHT_ERROR: f64 = 0.25;
/// 决策信号词命中加成(低于错误)。
pub const SALIENCE_SIGNAL_WEIGHT_DECISION: f64 = 0.2;
/// 内容信号总加成上限(防止单一消息无限膨胀)。
pub const SALIENCE_CONTENT_BONUS_CAP: f64 = 1.0;

/// 结论强标记词 —— 命中即视为"已确认结论",salience 最高档。
const SALIENCE_STRONG_MARKERS: &[&str] = &[
    "根因是", "原因是", "确认", "已验证", "结论", "已修复", "修复了",
    "PASS", "FAIL", "found that", "verified", "root cause",
];
/// 错误信号词 —— 工具失败/异常结果。
const SALIENCE_ERROR_MARKERS: &[&str] = &[
    "error", "panic", "fail", "failed", "报错", "失败", "异常",
];
/// 决策信号词 —— 决策/方案陈述。
const SALIENCE_DECISION_MARKERS: &[&str] = &[
    "decided", "decision", "决定", "方案", "alternatives",
];
```

在自由函数区(`rrf_merge` 之后)新增:

```rust
/// 规则式 salience 打分 —— 返回乘子(≥1.0),作用于 RRF 融合分数。
///
/// `final_rank = rrf_score × salience_weight(role, content)`。
/// 由角色基值 + 内容信号词加成组成;内容加成封顶 [`SALIENCE_CONTENT_BONUS_CAP`]。
/// 信号词匹配大小写不敏感。
#[must_use]
fn salience_weight(role: &str, content: &str) -> f64 {
    let base = match role {
        "decision" => SALIENCE_ROLE_DECISION,
        "assistant" => SALIENCE_ROLE_ASSISTANT,
        _ => SALIENCE_ROLE_BASELINE,
    };
    let lower = content.to_ascii_lowercase();
    let count_marker = |markers: &[&str], weight: f64| -> f64 {
        markers
            .iter()
            .filter(|m| lower.contains(&m.to_ascii_lowercase()))
            .count() as f64
            * weight
    };
    let bonus = count_marker(SALIENCE_STRONG_MARKERS, SALIENCE_SIGNAL_WEIGHT)
        + count_marker(SALIENCE_ERROR_MARKERS, SALIENCE_SIGNAL_WEIGHT_ERROR)
        + count_marker(SALIENCE_DECISION_MARKERS, SALIENCE_SIGNAL_WEIGHT_DECISION);
    base + bonus.min(SALIENCE_CONTENT_BONUS_CAP)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::salience_weight_`
Expected: 4 个测试全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): salience_weight 规则式显著性打分(角色基值 + 内容信号词)"
```

---

## Task 2: `hybrid_search` 集成 salience 层

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs`(`hybrid_search` 尾部)

- [ ] **Step 1: 写失败测试**

在测试模块追加:

```rust
    #[test]
    fn hybrid_search_applies_salience_boost_to_decision() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider.clone());
        // 相同词袋内容:一条 user、一条 decision —— 词法 rank 相同(decision 在 search()
        // 内部 ×2.0,融合后已略高);salience 层放大决策点差距。
        index
            .index_message("rust toolchain decision", "s1", "user", 0, 1_000)
            .expect("index user");
        index
            .index_message("rust toolchain decision", "s1", "decision", 1, 2_000)
            .expect("index decision");
        let hits = index.hybrid_search("rust toolchain", 5).expect("hybrid");
        assert_eq!(hits.len(), 2, "both messages should be returned");
        let decision_pos = hits
            .iter()
            .position(|h| h.role == "decision")
            .expect("decision hit present");
        let user_pos = hits.iter().position(|h| h.role == "user").expect("user hit present");
        assert!(
            decision_pos < user_pos,
            "decision should rank above user after salience reweight: {decision_pos} vs {user_pos}"
        );
    }

    #[test]
    fn hybrid_search_boosts_conclusion_heavy_assistant_message() {
        use crate::memory_semantic::HashEmbeddingProvider;
        let (_file, index) = open_temp_index();
        let provider: Arc<dyn EmbeddingProvider + Send + Sync> =
            Arc::new(HashEmbeddingProvider::default_dim());
        let index = index.with_embedder(provider);
        // 两条 assistant 消息都命中词法查询,但一条含结论强标记
        index
            .index_message("rust toolchain setup complete", "s1", "assistant", 0, 1_000)
            .expect("index plain");
        index
            .index_message("rust toolchain root cause verified, PASS", "s1", "assistant", 1, 2_000)
            .expect("index conclusion");
        let hits = index.hybrid_search("rust toolchain", 5).expect("hybrid");
        assert_eq!(hits.len(), 2);
        let conclusion_pos = hits
            .iter()
            .position(|h| h.message_index == 1)
            .expect("conclusion hit");
        let plain_pos = hits
            .iter()
            .position(|h| h.message_index == 0)
            .expect("plain hit");
        assert!(
            conclusion_pos < plain_pos,
            "conclusion-heavy message should rank above plain: {conclusion_pos} vs {plain_pos}"
        );
    }

    #[test]
    fn hybrid_search_without_embedder_skips_salience() {
        // 无 embedder:hybrid_search 直接返回词法结果,不应用 salience。
        let (_file, index) = open_temp_index();
        index
            .index_message("rust toolchain decision", "s1", "decision", 0, 1_000)
            .expect("index");
        let hits = index.hybrid_search("rust", 5).expect("hybrid");
        assert!(!hits.is_empty(), "lexical fallback should still work");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p runtime history_search::tests::hybrid_search_applies_salience_boost_to_decision`
Expected: FAIL 或 PASS——`hybrid_search` 已存在但尚未集成 salience，断言可能失败（decision 仅靠词法 ×2.0 排前，无法证明 salience 生效）。为确定性，先跑 Task 1 后再看本测试：若当前实现已让它 PASS（decision ×2.0 足够拉开差距），则说明测试不够敏感——此时把 user/decision 词袋改为**完全相同且同时命中 dense**，使词法差距最小化。**实现目标：salience 层必须显著改变排名**。

- [ ] **Step 3: 实现(在 `hybrid_search` 返回前加 salience 层)**

将 `hybrid_search` 尾部的:

```rust
        if dense.is_empty() {
            return Ok(lexical);
        }
        Ok(rrf_merge(lexical, dense, top_k))
```

替换为:

```rust
        if dense.is_empty() {
            return Ok(lexical);
        }
        let mut merged = rrf_merge(lexical, dense, top_k);
        // Phase 2:salience 重加权(L3 salience reweighter)。
        // final_rank = rrf_score × salience_weight(role, content)。
        // 仅融合路径生效:无 embedder / dense 为空时直接返回词法结果,不应用。
        for hit in &mut merged {
            hit.rank *= salience_weight(&hit.role, &hit.content);
        }
        merged.sort_by(|a, b| {
            b.rank
                .partial_cmp(&a.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(merged)
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p runtime history_search::tests::hybrid_search_`
Run: `cargo test -p runtime history_search`
Expected: 全部 PASS(新增 3 + 既有 26 = 29 个)。

- [ ] **Step 5: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "feat(runtime): hybrid_search 融合结果应用 salience 重加权"
```

---

## Task 3: 全量回归

**Files:** 无(验证)

- [ ] **Step 1: runtime 全量测试**

Run: `cargo test -p runtime`
Expected: 全量 PASS(1780 + 新增 ~7 = 1787，零回归)。

- [ ] **Step 2: 无 embedding feature 构建回归**

Run: `cargo build -p runtime`
Expected: 编译通过(证明 salience 层不依赖 embedding feature；无 embedder 时 `hybrid_search` 直接返回词法结果)。

- [ ] **Step 3: 提交(如有遗留修改)**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "test(runtime): salience 重加权回归验证"
```

---

## Self-Review 清单

- **Spec 覆盖**：salience 打分函数(Task 1) + 融合路径集成(Task 2) + 回归(Task 3)完整覆盖 Phase 2 目标。回退安全(Task 2 Step 3 的 `dense.is_empty()` 分支不变)与 Phase 1 承诺一致。
- **占位符扫描**：无 TBD；每个代码步骤含完整可编译代码。
- **类型一致性**：`salience_weight` 在 Task 1 定义 `(role: &str, content: &str) -> f64`，Task 2 一致调用；常量名在 Task 1 定义、Task 2 不直接引用(仅函数内)；`hybrid_search` 签名不变(仍 `(query, top_k) -> Result<Vec<HistoryHit>>`)，调用方 `execute_session_search` 零改动。
