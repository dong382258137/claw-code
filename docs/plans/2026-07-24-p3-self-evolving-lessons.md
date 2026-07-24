# P3 自进化模块（Lessons Distiller）实施 Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 NOTEBOOK 工作记忆中新增 `<lessons>` 段，复用 nudge 周期触发模式从失败/成功轨迹提取结构化 lessons，让 agent 在不修改模型权重的前提下从经验中学习。

**Architecture:** 混合方案 = ExpeL（经验提取）+ Self-Harness（外部信号门控）+ ACE（结构化增量条目）。新增 `lessons_distiller.rs` 模块，复用 `notebook.rs` 的段机制和 `conversation.rs:1792` 的 nudge 触发模板。lessons 通过 `render_for_prompt` 自动注入 system_prompt 变动区，跨压缩持久化。

**Tech Stack:** Rust, runtime crate, 现有 NOTEBOOK/DecisionLog/TraceAnalyzer 基础设施。无外部依赖，无 LLM 调用（规则式提取）。

---

## 论文依据

| 论文 | 借鉴点 |
|---|---|
| Self-Harness (arXiv:2606.09498, 上海 AI Lab 2026-06) | 失败聚类按机制签名；保守晋升防过拟合；模型不动只改 harness |
| ExpeL (AAAI 2024) | 经验提取 → insights 维护 → 推理时召回；ADD/EDIT/UPVOTE/DOWNVOTE 动作模型 |
| ACE / Agentic Context Engineering (Zhang et al. 2025) | Generator/Reflector/Curator 三组件；Curator 输出结构化 (id, desc) 增量条目 |
| Misevolution (arXiv:2509.26354, 2025-09) | 99.3% 自我改进是有界自优化；评估器必须用外部信号，禁止纯 LLM 自评 |

## 项目基础设施盘点

| 设施 | 文件 | 复用方式 |
|---|---|---|
| NOTEBOOK 5 段 | `runtime/src/notebook.rs` | 扩展 SECTION_TAGS 加入 `lessons` |
| nudge 触发模式 | `runtime/src/conversation.rs:1792-1855` | 复用 `turns_since_last_nudge` 计数器模式 |
| NudgeAction 动作模型 | `runtime/src/memory.rs:833-849` | LessonAction 借鉴 Add/Replace/Remove |
| TraceRecord 失败记录 | `runtime/src/trace_analyzer.rs:52-96` | 失败轨迹数据源 |
| record_turn_failed | `runtime/src/conversation.rs:2832-2851` | 失败信号采集点 |
| DecisionLog success_rate 学习环 | `runtime/src/decision_log.rs:562-721` | Phase 2 保守晋升目标 |

## 关键设计决策

1. **不动模型权重** — 上下文层进化（最易实施，ROI 最高）
2. **规则式提取，无 LLM 调用** — MVP 阶段用关键词/模式匹配，避免 misevolution 风险
3. **外部信号门控** — lesson 必须关联可验证信号（工具失败、编译错误、测试失败）
4. **lessons 段容量限制** — 最多 20 条，LRU 淘汰，防膨胀
5. **去重** — 基于条件文本的精确匹配去重（simhash 留到 Phase 2）
6. **不自动晋升到 DecisionLog** — Phase 2 才实现，MVP 只写 NOTEBOOK

## Lesson 条目格式

```text
[confidence:0-5] condition → action (signal:xxx)
```

示例：
```text
[3] cargo build 失败且错误含 "cannot find value" → 检查变量是否在正确作用域声明 (signal:cargo_build_fail)
[2] Edit 工具返回 "old_string not found" → 先用 Read 确认当前文件内容再编辑 (signal:edit_old_string_not_found)
[4] TUI 模式下 println 导致屏幕损坏 → 用 tui_mode flag 门控所有 stdout 输出 (signal:tui_screen_corruption)
```

- `confidence` 0-5 整数，初始为 1-2，复现 +1，被纠正 -1
- `condition` 触发条件描述
- `action` 建议动作
- `signal` 关联的外部信号标识

## File Structure

| 文件 | 操作 | 责任 |
|---|---|---|
| `runtime/src/notebook.rs` | 修改 | SECTION_TAGS 加入 `lessons`；NOTEBOOK_HEADER 更新；NOTEBOOK_UPDATE_TOOL_SPEC 更新 |
| `runtime/src/lessons_distiller.rs` | 新建 | LessonAction 类型、LessonsConfig、should_distill_lessons、extract_lesson_actions、去重 + 容量控制 |
| `runtime/src/conversation.rs` | 修改 | 在 nudge 块后添加 lessons 蒸馏触发；新增 turns_since_last_distill 字段 |
| `runtime/src/lib.rs` | 修改 | 导出 lessons_distiller 模块 |
| `runtime/src/lessons_distiller.rs` (tests) | 新建 | 单元测试 + 端到端测试 |

---

## Task 1: 扩展 NOTEBOOK 加入 `<lessons>` 段

**Files:**
- Modify: `runtime/src/notebook.rs:81` (SECTION_TAGS)
- Modify: `runtime/src/notebook.rs:84-94` (NOTEBOOK_HEADER)
- Modify: `runtime/src/notebook.rs:366-389` (NOTEBOOK_UPDATE_TOOL_SPEC)
- Test: `runtime/src/notebook.rs` (tests 模块)

- [ ] **Step 1: 写失败测试 — SECTION_TAGS 包含 lessons**

在 `runtime/src/notebook.rs` 的 `tests` 模块末尾添加：

```rust
#[test]
fn section_tags_includes_lessons() {
    assert!(SECTION_TAGS.contains(&"lessons"));
}
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cargo test -p runtime --lib notebook::tests::section_tags_includes_lessons`
Expected: FAIL with `assertion failed`

- [ ] **Step 3: 修改 SECTION_TAGS**

`runtime/src/notebook.rs:81`：

```rust
pub const SECTION_TAGS: &[&str] = &["plan", "subagents", "attempted", "preferences", "key_files", "lessons"];
```

- [ ] **Step 4: 更新 NOTEBOOK_HEADER**

`runtime/src/notebook.rs:84-94`，在 `key_files` 行后追加 `lessons` 段说明：

```rust
pub const NOTEBOOK_HEADER: &str = "# NOTEBOOK — Structured Working Memory\n\
    \n\
    本文件是 AI 助手的工作记忆,跨压缩持久化。**microcompact / compact_session 不会影响本文件**。\n\
    通过 `notebook_update` 工具维护。每个 turn 开始时注入到 system_prompt 变动区。\n\
    \n\
    ## 段说明\n\
    - `<plan>`:当前任务的关键决策、约束、进度\n\
    - `<subagents>`:已 dispatch 的子智能体注册表(name | status | result_ref)\n\
    - `<attempted>`:已尝试的方案及结论(成功/失败 + 原因)\n\
    - `<preferences>`:用户明确表达的偏好/约束\n\
    - `<key_files>`:关键文件引用 + 一句话摘要\n\
    - `<lessons>`:从失败/成功轨迹蒸馏的经验教训(格式:`[confidence:0-5] condition → action (signal:xxx)`)\n";
```

- [ ] **Step 5: 更新 NOTEBOOK_UPDATE_TOOL_SPEC**

`runtime/src/notebook.rs:366-389`，修改 tool spec 的 section enum 和 description：

```rust
pub const NOTEBOOK_UPDATE_TOOL_SPEC: &str = r#"{
    "name": "notebook_update",
    "description": "Update the persistent working memory (NOTEBOOK.md). This memory survives context compaction — use it to record key decisions, subagent registry, attempted approaches, user preferences, key file references, and distilled lessons. CRITICAL: always record subagent dispatches here so you do not re-dispatch the same task later. Modes: 'set' (overwrite section) or 'append' (add a line). Sections: plan, subagents, attempted, preferences, key_files, lessons.",
    "input_schema": {
        "type": "object",
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["set", "append"],
                "description": "Operation mode: 'set' overwrites the entire section; 'append' adds a single line to the section."
            },
            "section": {
                "type": "string",
                "enum": ["plan", "subagents", "attempted", "preferences", "key_files", "lessons"],
                "description": "Target section name."
            },
            "content": {
                "type": "string",
                "description": "For 'set': full section content. For 'append': a single line to add (newline-terminated automatically)."
            }
        },
        "required": ["mode", "section", "content"]
    }
}"#;
```

- [ ] **Step 6: 写测试 — render_for_prompt 输出 lessons 段**

在 `tests` 模块添加：

```rust
#[test]
fn render_for_prompt_includes_lessons_section() {
    let mut nb = Notebook::new();
    nb.set_section("lessons", "[3] cargo build fail → check deps (signal:cargo_build_fail)");
    let rendered = nb.render_for_prompt();
    assert!(rendered.contains("<lessons>"));
    assert!(rendered.contains("cargo build fail"));
}

#[test]
fn parse_round_trip_with_lessons_section() {
    let mut nb = Notebook::new();
    nb.set_section("plan", "do something");
    nb.set_section("lessons", "[2] cond → act (signal:test)");
    let rendered = nb.render();
    let parsed = Notebook::parse(&rendered).expect("parse should succeed");
    assert_eq!(parsed.get_section("lessons"), Some("[2] cond → act (signal:test)"));
}
```

- [ ] **Step 7: 运行测试验证通过**

Run: `cargo test -p runtime --lib notebook`
Expected: PASS — 所有 notebook 测试通过（含新测试）

- [ ] **Step 8: Commit**

```bash
git add rust/crates/runtime/src/notebook.rs
git commit -m "feat(notebook): add <lessons> section for distilled experience lessons

- SECTION_TAGS 新增 'lessons' 段
- NOTEBOOK_HEADER 补充 lessons 段说明
- NOTEBOOK_UPDATE_TOOL_SPEC section enum 加入 'lessons'
- 支持格式: [confidence:0-5] condition → action (signal:xxx)

P3 自进化模块基础设施,借鉴 ExpeL/ACE 结构化经验条目。"
```

---

## Task 2: 创建 lessons_distiller.rs 模块骨架 + LessonAction 类型

**Files:**
- Create: `runtime/src/lessons_distiller.rs`
- Modify: `runtime/src/lib.rs` (导出模块)

- [ ] **Step 1: 写失败测试 — 模块存在且 LessonAction 可构造**

先创建测试文件 `runtime/src/lessons_distiller.rs`，只写测试骨架：

```rust
//! Lessons Distiller — P3 自进化模块 MVP。
//!
//! 从失败/成功轨迹蒸馏结构化 lessons,写入 NOTEBOOK `<lessons>` 段。
//!
//! 设计依据:
//! - ExpeL (AAAI 2024): 经验提取 → insights 维护 → 推理时召回
//! - Self-Harness (arXiv:2606.09498): 外部信号门控,防 misevolution
//! - ACE (Zhang et al. 2025): 结构化 (id, desc) 增量条目
//!
//! # 架构
//!
//! - [`LessonAction`]: 动作模型(Add/Replace/Remove),借鉴 NudgeAction
//! - [`LessonsConfig`]: 触发参数(间隔、lookback、容量上限)
//! - [`should_distill_lessons`]: 触发判断(间隔 + 有失败信号)
//! - [`extract_lesson_actions`]: 规则式提取(无 LLM 调用)
//!
//! # Lesson 格式
//!
//! ```text
//! [confidence:0-5] condition → action (signal:xxx)
//! ```
//!
//! # 不变量
//!
//! - **无 LLM 调用**:纯规则提取,避免 misevolution(自评奖励黑客)
//! - **外部信号门控**:每条 lesson 必须关联一个 signal 标识
//! - **容量限制**:lessons 段最多 `MAX_LESSONS_ENTRIES` 条,LRU 淘汰
//! - **去重**:基于 condition 文本精确匹配

use crate::conversation::ConversationMessage;
use crate::memory::{MessageRole, NudgeAction};

/// lessons 段最大条目数,防止膨胀。
pub const MAX_LESSONS_ENTRIES: usize = 20;

/// lesson confidence 初始值(新蒸馏的 lesson 从 1 开始)。
pub const INITIAL_CONFIDENCE: u8 = 1;

/// lesson confidence 上限。
pub const MAX_CONFIDENCE: u8 = 5;

/// Lessons 蒸馏配置。
#[derive(Debug, Clone)]
pub struct LessonsConfig {
    /// 触发间隔(轮)。每 N 轮尝试蒸馏一次。
    pub interval_turns: usize,
    /// 回看轮数。提取最近 N 个 user turn 的消息。
    pub lookback_turns: usize,
    /// 每次蒸馏最多新增的 lesson 条数。
    pub max_entries_per_distill: usize,
}

impl Default for LessonsConfig {
    fn default() -> Self {
        Self {
            interval_turns: 5,
            lookback_turns: 3,
            max_entries_per_distill: 2,
        }
    }
}

/// Lessons 蒸馏动作(借鉴 NudgeAction,带 confidence 和 signal)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LessonAction {
    /// 新增 lesson。condition 唯一标识一条 lesson。
    Add {
        condition: String,
        action: String,
        signal: String,
        confidence: u8,
    },
    /// 已有 lesson 再次复现,confidence +1(上限 MAX_CONFIDENCE)。
    Reinforce {
        condition: String,
        signal: String,
    },
    /// 已有 lesson 被纠正(用户明确否定),confidence -1;降到 0 时应 Remove。
    Demote {
        condition: String,
        signal: String,
    },
    /// 移除 lesson(confidence 降到 0 或用户明确 forget)。
    Remove {
        condition: String,
    },
}

/// 把 LessonAction 渲染为 NOTEBOOK `<lessons>` 段的一行文本。
///
/// 仅 `Add` 和 `Reinforce` 产生实际写入行;`Demote`/`Remove` 由调用方
/// 在写入前过滤(它们是控制信号,不直接产生文本行)。
#[must_use]
pub fn render_lesson_line(action: &LessonAction) -> Option<String> {
    match action {
        LessonAction::Add {
            condition,
            action,
            signal,
            confidence,
        } => Some(format!(
            "[{confidence}] {condition} → {action} (signal:{signal})"
        )),
        LessonAction::Reinforce { condition, signal } => {
            // Reinforce 不直接产生新行,调用方应更新已有行的 confidence。
            // 这里返回 None,调用方按 condition 查找已有行并 +1。
            let _ = (condition, signal);
            None
        }
        LessonAction::Demote { condition, signal } => {
            let _ = (condition, signal);
            None
        }
        LessonAction::Remove { condition } => {
            let _ = condition;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lesson_action_add_renders_to_line() {
        let action = LessonAction::Add {
            condition: "cargo build fail".to_string(),
            action: "check deps".to_string(),
            signal: "cargo_build_fail".to_string(),
            confidence: 2,
        };
        let line = render_lesson_line(&action);
        assert_eq!(
            line,
            Some("[2] cargo build fail → check deps (signal:cargo_build_fail)".to_string())
        );
    }

    #[test]
    fn lesson_action_reinforce_renders_none() {
        let action = LessonAction::Reinforce {
            condition: "x".to_string(),
            signal: "y".to_string(),
        };
        assert!(render_lesson_line(&action).is_none());
    }

    #[test]
    fn lessons_config_default_sensible() {
        let c = LessonsConfig::default();
        assert_eq!(c.interval_turns, 5);
        assert_eq!(c.lookback_turns, 3);
        assert_eq!(c.max_entries_per_distill, 2);
    }
}
```

- [ ] **Step 2: 在 lib.rs 导出模块**

`runtime/src/lib.rs` 找到 `pub mod notebook;` 行附近，添加：

```rust
pub mod lessons_distiller;
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test -p runtime --lib lessons_distiller`
Expected: PASS — 3 个测试通过

- [ ] **Step 4: Commit**

```bash
git add rust/crates/runtime/src/lessons_distiller.rs rust/crates/runtime/src/lib.rs
git commit -m "feat(lessons): add lessons_distiller module skeleton

- LessonAction 枚举(Add/Reinforce/Demote/Remove),借鉴 NudgeAction
- LessonsConfig 触发参数(interval/lookback/max_entries)
- render_lesson_line 渲染为 NOTEBOOK <lessons> 段格式
- 模块导出到 runtime crate

P3 自进化模块 MVP 骨架,无 LLM 调用,纯规则式提取。"
```

---

## Task 3: 实现 should_distill_lessons() 触发条件

**Files:**
- Modify: `runtime/src/lessons_distiller.rs`

**设计:** 触发条件 = (轮次间隔达标) AND (最近 lookback 轮内有失败信号)。失败信号 = trace_analyzer 记录的 failure_kind 或消息中含工具错误关键词。

- [ ] **Step 1: 写失败测试 — 间隔未达标不触发**

在 `lessons_distiller.rs` 的 `tests` 模块添加：

```rust
#[test]
fn should_distill_returns_false_when_interval_not_met() {
    let config = LessonsConfig::default();
    assert!(!should_distill_lessons(3, &[], &config));
}

#[test]
fn should_distill_returns_true_when_interval_met_and_has_failure() {
    let config = LessonsConfig::default();
    let failures = vec![("runtime_error".to_string(), "edit failed".to_string())];
    assert!(should_distill_lessons(5, &failures, &config));
}

#[test]
fn should_distill_returns_false_when_no_failure_signal() {
    let config = LessonsConfig::default();
    assert!(!should_distill_lessons(5, &[], &config));
}

#[test]
fn should_distill_returns_false_when_failures_empty() {
    let config = LessonsConfig::default();
    assert!(!should_distill_lessons(10, &[], &config));
}
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cargo test -p runtime --lib lessons_distiller::tests::should_distill`
Expected: FAIL — `should_distill_lessons` 未定义

- [ ] **Step 3: 实现 should_distill_lessons()**

在 `lessons_distiller.rs` 的 `LessonsConfig` impl 块后添加：

```rust
/// 失败信号类型:`(failure_kind, error_message)`。
pub type FailureSignal = (String, String);

/// 判断是否应该触发 lessons 蒸馏。
///
/// 触发条件(两者都满足):
/// 1. `turns_since_last_distill >= config.interval_turns`
/// 2. `recent_failures` 非空(至少有一个失败信号)
///
/// # 参数
///
/// - `turns_since_last_distill` — 距上次蒸馏的轮次数
/// - `recent_failures` — 最近 lookback 轮内的失败信号列表
/// - `config` — 蒸馏配置
#[must_use]
pub fn should_distill_lessons(
    turns_since_last_distill: usize,
    recent_failures: &[FailureSignal],
    config: &LessonsConfig,
) -> bool {
    turns_since_last_distill >= config.interval_turns && !recent_failures.is_empty()
}
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cargo test -p runtime --lib lessons_distiller::tests::should_distill`
Expected: PASS — 4 个测试通过

- [ ] **Step 5: Commit**

```bash
git add rust/crates/runtime/src/lessons_distiller.rs
git commit -m "feat(lessons): implement should_distill_lessons trigger

触发条件:轮次间隔达标 AND 最近有失败信号。
失败信号为 (failure_kind, error_message) 元组列表。
借鉴 nudge 周期触发模式,但增加失败信号门控。"
```

---

## Task 4: 实现 extract_lesson_actions() 规则式提取

**Files:**
- Modify: `runtime/src/lessons_distiller.rs`

**设计:** 规则式提取,扫描最近消息和失败信号,匹配预定义的错误模式 → lesson 映射。无 LLM 调用,避免 misevolution。

预定义模式库（MVP 覆盖常见场景）：

| 错误模式 | signal | condition | action |
|---|---|---|---|
| `old_string not found` (Edit 工具) | edit_old_string_not_found | Edit 工具返回 old_string 未找到 | 先用 Read 确认当前文件内容再编辑 |
| `cannot find value` (Rust 编译) | rust_cannot_find_value | cargo build 报 cannot find value | 检查变量是否在正确作用域声明 |
| `unresolved import` (Rust 编译) | rust_unresolved_import | cargo build 报 unresolved import | 检查 Cargo.toml dependencies 和 use 路径 |
| `connection refused` / `ECONNREFUSED` | network_connection_refused | 工具调用报连接拒绝 | 检查目标服务是否启动,端口是否正确 |
| `Permission denied` | fs_permission_denied | 文件操作报权限拒绝 | 检查文件权限,必要时用管理员权限 |
| `No such file or directory` | fs_not_found | 文件操作报文件不存在 | 先用 ls/glob 确认路径再操作 |

- [ ] **Step 1: 写失败测试 — 匹配 edit_old_string_not_found 模式**

在 `lessons_distiller.rs` 的 `tests` 模块添加：

```rust
fn make_user_message(text: &str) -> ConversationMessage {
    use crate::conversation::ContentBlock;
    ConversationMessage {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        tool_use_id: None,
        tool_result_id: None,
        timestamp_ms: 0,
    }
}

#[test]
fn extract_lessons_matches_edit_old_string_not_found() {
    let config = LessonsConfig::default();
    let failures = vec![(
        "tool_error".to_string(),
        "Edit failed: old_string not found in file".to_string(),
    )];
    let messages = vec![make_user_message("fix the bug")];
    let actions = extract_lesson_actions(&messages, &failures, &config);
    assert!(actions.iter().any(|a| match a {
        LessonAction::Add { signal, .. } => signal == "edit_old_string_not_found",
        _ => false,
    }));
}

#[test]
fn extract_lessons_matches_rust_cannot_find_value() {
    let config = LessonsConfig::default();
    let failures = vec![(
        "runtime_error".to_string(),
        "error[E0425]: cannot find value `x` in this scope".to_string(),
    )];
    let messages: Vec<ConversationMessage> = vec![];
    let actions = extract_lesson_actions(&messages, &failures, &config);
    assert!(actions.iter().any(|a| match a {
        LessonAction::Add { signal, .. } => signal == "rust_cannot_find_value",
        _ => false,
    }));
}

#[test]
fn extract_lessons_returns_empty_when_no_match() {
    let config = LessonsConfig::default();
    let failures = vec![("unknown".to_string(), "some random error".to_string())];
    let messages: Vec<ConversationMessage> = vec![];
    let actions = extract_lesson_actions(&messages, &failures, &config);
    assert!(actions.is_empty());
}

#[test]
fn extract_lessons_respects_max_entries_limit() {
    let config = LessonsConfig {
        max_entries_per_distill: 1,
        ..Default::default()
    };
    let failures = vec![
        ("e1".to_string(), "old_string not found".to_string()),
        ("e2".to_string(), "cannot find value `x`".to_string()),
    ];
    let messages: Vec<ConversationMessage> = vec![];
    let actions = extract_lesson_actions(&messages, &failures, &config);
    assert!(actions.len() <= 1);
}
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cargo test -p runtime --lib lessons_distiller::tests::extract_lessons`
Expected: FAIL — `extract_lesson_actions` 未定义，`ConversationMessage` 字段可能不匹配

- [ ] **Step 3: 实现 extract_lesson_actions()**

在 `lessons_distiller.rs` 的 `should_distill_lessons` 后添加：

```rust
/// 错误模式 → lesson 映射规则。
///
/// 每条规则:(关键词, signal, condition, action)
/// 关键词在 error_message 中匹配(大小写不敏感)即触发。
const LESSON_PATTERNS: &[(&str, &str, &str, &str)] = &[
    (
        "old_string not found",
        "edit_old_string_not_found",
        "Edit 工具返回 old_string 未找到",
        "先用 Read 确认当前文件内容再编辑,避免基于过期内容匹配",
    ),
    (
        "cannot find value",
        "rust_cannot_find_value",
        "cargo build 报 cannot find value",
        "检查变量是否在正确作用域声明,或是否缺少 use 导入",
    ),
    (
        "unresolved import",
        "rust_unresolved_import",
        "cargo build 报 unresolved import",
        "检查 Cargo.toml dependencies 是否齐全,use 路径是否正确",
    ),
    (
        "connection refused",
        "network_connection_refused",
        "工具调用报连接拒绝",
        "检查目标服务是否启动,端口是否正确,防火墙是否放行",
    ),
    (
        "econnrefused",
        "network_connection_refused",
        "工具调用报连接拒绝",
        "检查目标服务是否启动,端口是否正确,防火墙是否放行",
    ),
    (
        "permission denied",
        "fs_permission_denied",
        "文件操作报权限拒绝",
        "检查文件权限,必要时用管理员权限或 chmod 调整",
    ),
    (
        "no such file or directory",
        "fs_not_found",
        "文件操作报文件不存在",
        "先用 ls/glob 确认路径存在再操作,注意大小写和相对路径基准",
    ),
];

/// 从失败信号 + 消息历史提取 lesson 动作。
///
/// 规则式提取(无 LLM 调用):
/// 1. 遍历 `recent_failures`,对每个 (failure_kind, error_message)
/// 2. 在 `LESSON_PATTERNS` 中找首个关键词命中的规则
/// 3. 生成 `LessonAction::Add`(confidence = INITIAL_CONFIDENCE)
/// 4. 去重:同一 signal 只生成一条(首个命中)
/// 5. 截断到 `config.max_entries_per_distill`
///
/// # 参数
///
/// - `recent_messages` — 最近 lookback 轮的消息(当前 MVP 未深度使用,预留)
/// - `recent_failures` — 失败信号列表
/// - `config` — 蒸馏配置
#[must_use]
pub fn extract_lesson_actions(
    _recent_messages: &[ConversationMessage],
    recent_failures: &[FailureSignal],
    config: &LessonsConfig,
) -> Vec<LessonAction> {
    let mut actions = Vec::new();
    let mut seen_signals: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (_kind, error_msg) in recent_failures {
        if actions.len() >= config.max_entries_per_distill {
            break;
        }
        let lower = error_msg.to_lowercase();
        for (keyword, signal, condition, action) in LESSON_PATTERNS {
            if lower.contains(keyword) && seen_signals.insert((*signal).to_string()) {
                actions.push(LessonAction::Add {
                    condition: (*condition).to_string(),
                    action: (*action).to_string(),
                    signal: (*signal).to_string(),
                    confidence: INITIAL_CONFIDENCE,
                });
                break; // 每个 failure 只匹配首个规则
            }
        }
    }

    actions
}
```

- [ ] **Step 4: 修复测试中的 ConversationMessage 构造**

如果 `ConversationMessage` 的实际字段与测试中 `make_user_message` 不匹配，需要调整测试。先检查 `runtime/src/conversation.rs` 中 `ConversationMessage` 的定义，按实际字段调整。常见情况：字段名可能是 `content: Vec<ContentBlock>` 或 `blocks`，`ContentBlock::Text` 可能是 `ContentBlock::Text { text }` 或其他变体。

运行 `cargo check -p runtime` 确认编译通过。如果测试构造代码不匹配，调整为实际字段名。

- [ ] **Step 5: 运行测试验证通过**

Run: `cargo test -p runtime --lib lessons_distiller::tests::extract_lessons`
Expected: PASS — 4 个测试通过

- [ ] **Step 6: Commit**

```bash
git add rust/crates/runtime/src/lessons_distiller.rs
git commit -m "feat(lessons): implement extract_lesson_actions rule-based extraction

- LESSON_PATTERNS 预定义 7 种错误模式 → lesson 映射
- 覆盖:Edit old_string/Rust 编译/网络/文件系统常见错误
- 规则式提取,无 LLM 调用,防 misevolution
- signal 去重,同一 signal 只生成一条
- max_entries_per_distill 截断

借鉴 ExpeL 经验提取 + Self-Harness 外部信号门控。"
```

---

## Task 5: 实现 lessons 段去重 + 容量控制

**Files:**
- Modify: `runtime/src/lessons_distiller.rs`

**设计:** 把 LessonAction 列表应用到 NOTEBOOK 现有 lessons 段，处理去重、Reinforce/Demote/Remove、容量 LRU 淘汰。

- [ ] **Step 1: 写失败测试 — Add 新 lesson 到空段**

在 `lessons_distiller.rs` 的 `tests` 模块添加：

```rust
#[test]
fn apply_actions_adds_new_lesson_to_empty_section() {
    let actions = vec![LessonAction::Add {
        condition: "cond1".to_string(),
        action: "act1".to_string(),
        signal: "sig1".to_string(),
        confidence: 1,
    }];
    let result = apply_lesson_actions("", &actions);
    assert!(result.contains("[1] cond1 → act1 (signal:sig1)"));
}

#[test]
fn apply_actions_dedupes_by_condition() {
    let existing = "[1] cond1 → act1 (signal:sig1)";
    let actions = vec![LessonAction::Add {
        condition: "cond1".to_string(),
        action: "act1".to_string(),
        signal: "sig1".to_string(),
        confidence: 1,
    }];
    let result = apply_lesson_actions(existing, &actions);
    // 同 condition 不重复添加
    let count = result.matches("cond1").count();
    assert_eq!(count, 1);
}

#[test]
fn apply_actions_enforces_max_entries() {
    let mut existing = String::new();
    for i in 0..MAX_LESSONS_ENTRIES {
        existing.push_str(&format!(
            "[1] cond{i} → act{i} (signal:sig{i})\n"
        ));
    }
    let actions = vec![LessonAction::Add {
        condition: "new_cond".to_string(),
        action: "new_act".to_string(),
        signal: "new_sig".to_string(),
        confidence: 1,
    }];
    let result = apply_lesson_actions(&existing, &actions);
    let line_count = result.lines().count();
    assert!(line_count <= MAX_LESSONS_ENTRIES);
}

#[test]
fn apply_actions_reinforce_increments_confidence() {
    let existing = "[2] cond1 → act1 (signal:sig1)";
    let actions = vec![LessonAction::Reinforce {
        condition: "cond1".to_string(),
        signal: "sig1".to_string(),
    }];
    let result = apply_lesson_actions(existing, &actions);
    assert!(result.contains("[3] cond1 → act1 (signal:sig1)"));
}

#[test]
fn apply_actions_reinforce_caps_at_max_confidence() {
    let existing = "[5] cond1 → act1 (signal:sig1)";
    let actions = vec![LessonAction::Reinforce {
        condition: "cond1".to_string(),
        signal: "sig1".to_string(),
    }];
    let result = apply_lesson_actions(existing, &actions);
    assert!(result.contains("[5] cond1 → act1 (signal:sig1)"));
    assert!(!result.contains("[6]"));
}

#[test]
fn apply_actions_remove_deletes_lesson() {
    let existing = "[1] cond1 → act1 (signal:sig1)\n[2] cond2 → act2 (signal:sig2)";
    let actions = vec![LessonAction::Remove {
        condition: "cond1".to_string(),
    }];
    let result = apply_lesson_actions(existing, &actions);
    assert!(!result.contains("cond1"));
    assert!(result.contains("cond2"));
}
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cargo test -p runtime --lib lessons_distiller::tests::apply_actions`
Expected: FAIL — `apply_lesson_actions` 未定义

- [ ] **Step 3: 实现 apply_lesson_actions()**

在 `lessons_distiller.rs` 的 `extract_lesson_actions` 后添加：

```rust
/// 解析 lessons 段的一行,提取 (confidence, condition, action, signal)。
///
/// 行格式:`[N] condition → action (signal:xxx)`
/// 解析失败返回 None(容错,跳过格式错误的行)。
fn parse_lesson_line(line: &str) -> Option<(u8, String, String, String)> {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') {
        return None;
    }
    let close_bracket = trimmed.find(']')?;
    let confidence_str = &trimmed[1..close_bracket];
    let confidence: u8 = confidence_str.parse().ok()?;
    let rest = trimmed[close_bracket + 1..].trim();
    let arrow_idx = rest.find("→")?;
    let condition = rest[..arrow_idx].trim().to_string();
    let after_arrow = rest[arrow_idx + "→".len()..].trim();
    let signal_idx = after_arrow.rfind("(signal:")?;
    let action = after_arrow[..signal_idx].trim().to_string();
    let signal_end = after_arrow.rfind(')')?;
    let signal = after_arrow[signal_idx + "(signal:".len()..signal_end].trim().to_string();
    Some((confidence, condition, action, signal))
}

/// 把 LessonAction 列表应用到现有 lessons 段内容,返回新内容。
///
/// 流程:
/// 1. 解析现有内容为 (confidence, condition, action, signal) 列表
/// 2. 对每个 action:
///    - Add:condition 已存在则跳过(去重),否则追加
///    - Reinforce:匹配 condition,confidence +1(上限 MAX_CONFIDENCE)
///    - Demote:匹配 condition,confidence -1;降到 0 标记为待删除
///    - Remove:匹配 condition,删除
/// 3. 容量控制:超过 MAX_LESSONS_ENTRIES 时,按 confidence 升序淘汰(低优先淘汰)
/// 4. 重新渲染为多行文本
#[must_use]
pub fn apply_lesson_actions(existing: &str, actions: &[LessonAction]) -> String {
    // 解析现有 lessons 为可变列表
    let mut entries: Vec<(u8, String, String, String)> = Vec::new();
    for line in existing.lines() {
        if let Some(parsed) = parse_lesson_line(line) {
            entries.push(parsed);
        }
    }

    for action in actions {
        match action {
            LessonAction::Add {
                condition,
                action,
                signal,
                confidence,
            } => {
                // 去重:condition 已存在则跳过
                let exists = entries.iter().any(|(_, c, _, _)| c == condition);
                if !exists {
                    entries.push((*confidence, condition.clone(), action.clone(), signal.clone()));
                }
            }
            LessonAction::Reinforce { condition, signal: _ } => {
                for entry in &mut entries {
                    if entry.1 == *condition && entry.0 < MAX_CONFIDENCE {
                        entry.0 += 1;
                    }
                }
            }
            LessonAction::Demote { condition, signal: _ } => {
                for entry in &mut entries {
                    if entry.1 == *condition && entry.0 > 0 {
                        entry.0 -= 1;
                    }
                }
                // confidence 降到 0 的标记为待删除
                entries.retain(|(c, _, _, _)| *c > 0);
            }
            LessonAction::Remove { condition } => {
                entries.retain(|(_, c, _, _)| c != condition);
            }
        }
    }

    // 容量控制:超过上限时按 confidence 升序淘汰
    if entries.len() > MAX_LESSONS_ENTRIES {
        entries.sort_by_key(|(c, _, _, _)| *c);
        let to_remove = entries.len() - MAX_LESSONS_ENTRIES;
        entries.drain(..to_remove);
    }

    // 按 condition 字典序排序(稳定输出,便于 diff)
    entries.sort_by(|a, b| a.1.cmp(&b.1));

    // 渲染
    entries
        .iter()
        .map(|(c, cond, act, sig)| format!("[{c}] {cond} → {act} (signal:{sig})"))
        .collect::<Vec<_>>()
        .join("\n")
}
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cargo test -p runtime --lib lessons_distiller::tests::apply_actions`
Expected: PASS — 6 个测试通过

- [ ] **Step 5: 运行全部 lessons_distiller 测试**

Run: `cargo test -p runtime --lib lessons_distiller`
Expected: PASS — 所有测试通过

- [ ] **Step 6: Commit**

```bash
git add rust/crates/runtime/src/lessons_distiller.rs
git commit -m "feat(lessons): implement apply_lesson_actions with dedup and LRU

- parse_lesson_line 解析 [N] cond → act (signal:xxx) 格式
- apply_lesson_actions 处理 Add/Reinforce/Demote/Remove
- 去重:同 condition 不重复添加
- 容量控制:超过 MAX_LESSONS_ENTRIES(20)按 confidence 升序淘汰
- Reinforce confidence +1 上限 5,Demote -1 降到 0 删除
- 输出按 condition 字典序排序,便于 diff

借鉴 ACE Curator 增量合并 + NudgeAction 动作模型。"
```

---

## Task 6: 在 conversation.rs 集成蒸馏触发 + 写入 NOTEBOOK

**Files:**
- Modify: `runtime/src/conversation.rs` (字段 + 蒸馏块)
- Modify: `runtime/src/conversation.rs` (导入)

**设计:** 在 `run_turn` 末尾的 nudge 块后添加 lessons 蒸馏块。复用 nudge 的 `turns_since_last_nudge` 模式,新增 `turns_since_last_distill` 字段。失败信号从 `self.session.messages` 最近的工具错误中提取。

- [ ] **Step 1: 写失败测试 — 蒸馏触发写入 NOTEBOOK lessons 段**

在 `runtime/src/conversation.rs` 的 `tests` 模块添加端到端测试。由于 `ConversationRuntime` 构造复杂，先写一个集成测试验证 `distill_lessons_into_notebook` 辅助函数：

```rust
#[test]
fn distill_lessons_writes_to_notebook_lessons_section() {
    use crate::lessons_distiller::{
        apply_lesson_actions, extract_lesson_actions, FailureSignal, LessonsConfig,
    };
    use crate::notebook::Notebook;

    // 模拟:失败信号 + 空消息
    let failures: Vec<FailureSignal> = vec![(
        "tool_error".to_string(),
        "Edit failed: old_string not found".to_string(),
    )];
    let config = LessonsConfig::default();
    let actions = extract_lesson_actions(&[], &failures, &config);
    assert!(!actions.is_empty());

    // 应用到空 lessons 段
    let new_lessons = apply_lesson_actions("", &actions);
    assert!(new_lessons.contains("edit_old_string_not_found"));

    // 写入 NOTEBOOK 并验证 round-trip
    let mut nb = Notebook::new();
    nb.set_section("lessons", &new_lessons);
    let rendered = nb.render();
    let parsed = Notebook::parse(&rendered).expect("parse should succeed");
    assert_eq!(parsed.get_section("lessons"), Some(new_lessons.as_str()));
}
```

- [ ] **Step 2: 运行测试验证通过(应直接通过,因前序 task 已实现)**

Run: `cargo test -p runtime --lib conversation::tests::distill_lessons_writes_to_notebook_lessons_section`
Expected: PASS

- [ ] **Step 3: 在 ConversationRuntime 添加 turns_since_last_distill 字段**

找到 `runtime/src/conversation.rs` 中 `turns_since_last_nudge` 字段定义处（搜索 `turns_since_last_nudge`），在其后添加：

```rust
/// 距上次 lessons 蒸馏的轮次数。每 N 轮触发一次蒸馏(P3 自进化模块)。
turns_since_last_distill: usize,
```

在 `impl ConversationRuntime` 的 `new` 方法中初始化：

```rust
turns_since_last_distill: 0,
```

- [ ] **Step 4: 导入 lessons_distiller 模块**

在 `runtime/src/conversation.rs` 顶部 `use` 块中添加（搜索 `use crate::memory::` 附近）：

```rust
use crate::lessons_distiller::{
    apply_lesson_actions, extract_lesson_actions, should_distill_lessons, FailureSignal,
    LessonsConfig,
};
```

- [ ] **Step 5: 实现 distill_lessons 辅助方法**

在 `impl ConversationRuntime` 中（`record_turn_failed` 附近）添加私有方法：

```rust
/// P3 自进化模块:从最近失败信号蒸馏 lessons 写入 NOTEBOOK。
///
/// 在 run_turn 末尾调用,与 nudge 块并行(独立计数器)。
/// 流程:
/// 1. 检查 should_distill_lessons(轮次间隔 + 有失败信号)
/// 2. 从最近消息提取失败信号(工具错误/编译错误)
/// 3. extract_lesson_actions 规则式提取
/// 4. apply_lesson_actions 应用到 NOTEBOOK 现有 lessons 段
/// 5. 原子写回 NOTEBOOK.md
///
/// 无 LLM 调用,纯规则式,防 misevolution。
fn distill_lessons(&mut self) {
    if crate::poor_mode::is_active() {
        return;
    }
    self.turns_since_last_distill += 1;
    let config = LessonsConfig::default();
    if !should_distill_lessons(self.turns_since_last_distill, &[], &config) {
        return;
    }

    // 从最近消息提取失败信号(工具结果中的错误)
    let recent_failures = self.collect_recent_failure_signals(&config);
    if recent_failures.is_empty() {
        return;
    }

    let lookback_msgs: Vec<ConversationMessage> = self
        .session
        .messages
        .iter()
        .rev()
        .take(config.lookback_turns * 5)
        .cloned()
        .collect();

    let actions = extract_lesson_actions(&lookback_msgs, &recent_failures, &config);
    if actions.is_empty() {
        return;
    }

    // 加载现有 NOTEBOOK,应用到 lessons 段,写回
    let Some(workspace_root) = &self.workspace_root else {
        return;
    };
    let Ok(mut notebook) = crate::notebook::Notebook::load(workspace_root) else {
        return;
    };
    let existing_lessons = notebook
        .get_section("lessons")
        .unwrap_or("")
        .to_string();
    let new_lessons = apply_lesson_actions(&existing_lessons, &actions);
    notebook.set_section("lessons", &new_lessons);
    let _ = notebook.save(workspace_root);
    self.turns_since_last_distill = 0;
}

/// 从最近消息收集失败信号 (failure_kind, error_message)。
///
/// 扫描最近消息中的工具结果,提取 is_error=true 的工具输出。
/// 同时扫描 assistant 文本中含 "error" / "failed" 的片段。
fn collect_recent_failure_signals(&self, config: &LessonsConfig) -> Vec<FailureSignal> {
    let mut signals = Vec::new();
    let lookback = config.lookback_turns * 5; // 估算:1 turn ≈ 5 messages
    for msg in self.session.messages.iter().rev().take(lookback) {
        use crate::conversation::ContentBlock;
        for block in &msg.content {
            match block {
                ContentBlock::ToolResult { content, is_error, .. } if *is_error => {
                    let text = content
                        .iter()
                        .map(|c| match c {
                            ContentBlock::Text { text } => text.as_str(),
                            _ => "",
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        signals.push(("tool_error".to_string(), text));
                    }
                }
                _ => {}
            }
        }
    }
    signals
}
```

- [ ] **Step 6: 在 run_turn 末尾调用 distill_lessons**

找到 `runtime/src/conversation.rs:1855` nudge 块结束的 `}` 后（`Ok(summary)` 之前），添加：

```rust
// P3 自进化模块:lessons 蒸馏。与 nudge 独立计数器,复用触发模式。
// 无 LLM 调用,纯规则式提取,防 misevolution。
self.distill_lessons();
```

- [ ] **Step 7: 编译验证**

Run: `cargo check -p runtime`
Expected: 编译通过,无错误

如果 `ContentBlock::ToolResult` 的实际变体名或字段不匹配,根据 `runtime/src/conversation.rs` 中 `ContentBlock` 的实际定义调整 `collect_recent_failure_signals` 中的模式匹配。

- [ ] **Step 8: 运行全部测试验证无回归**

Run: `cargo test -p runtime --lib`
Expected: PASS — 所有测试通过(含原有 conversation/notebook/compact 测试)

- [ ] **Step 9: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "feat(conversation): integrate lessons distiller into run_turn

- 新增 turns_since_last_distill 字段(独立于 nudge 计数器)
- distill_lessons() 方法:触发判断 → 提取失败信号 → 规则式提取 → 写入 NOTEBOOK
- collect_recent_failure_signals() 从工具错误结果收集 (failure_kind, error_message)
- 在 run_turn 末尾 nudge 块后调用,poor_mode 短路保护
- 无 LLM 调用,纯规则式,防 misevolution

P3 自进化模块 MVP 集成完成。"
```

---

## Task 7: 全工作区编译 + 端到端验证

**Files:** 无修改,仅验证

- [ ] **Step 1: 全工作区编译**

Run: `cargo check --workspace`
Expected: 编译通过

- [ ] **Step 2: 运行所有相关模块测试**

Run:
```bash
cargo test -p runtime --lib lessons_distiller; cargo test -p runtime --lib notebook; cargo test -p runtime --lib compact; cargo test -p runtime --lib conversation
```
Expected: 全部 PASS

- [ ] **Step 3: 手动验证 lessons 段注入 system prompt**

检查 `runtime/src/conversation.rs` 中 NOTEBOOK 注入逻辑（搜索 `render_for_prompt`），确认 `lessons` 段会随其他段一起注入到 `system_split.dynamic_sections`。由于 Task 1 已把 `lessons` 加入 `SECTION_TAGS`,`render_for_prompt` 会自动包含它,无需额外修改。

验证：写一个单元测试确认含 lessons 的 NOTEBOOK 能渲染出完整 prompt 片段（已在 Task 1 Step 6 覆盖）。

- [ ] **Step 4: 运行 clippy(可选)**

Run: `cargo clippy -p runtime --lib 2>&1 | head -50`
Expected: 无新 warning(lessons_distiller 模块)

- [ ] **Step 5: 最终 commit(如有 lint 修复)**

```bash
git add -A
git commit -m "chore(lessons): final lint cleanup for P3 MVP"
```

---

## Self-Review 检查

### 1. Spec coverage

| 需求 | 覆盖 Task |
|---|---|
| NOTEBOOK 新增 `<lessons>` 段 | Task 1 |
| 复用 nudge 触发模式 | Task 6 (turns_since_last_distill) |
| LLM 从失败轨迹提取 lesson | Task 4 (规则式,非 LLM) |
| 写入 NOTEBOOK `<lessons>` 段 | Task 6 (distill_lessons) |
| 通过 render_for_prompt 自动注入 | Task 1 (SECTION_TAGS 扩展,自动生效) |
| 外部信号门控 | Task 4 (LESSON_PATTERNS 每条带 signal) |
| 容量限制 | Task 5 (MAX_LESSONS_ENTRIES + LRU) |
| 去重 | Task 5 (apply_lesson_actions 按 condition 去重) |
| 不动模型权重 | 全程无 LLM 调用,纯规则式 |
| 禁止纯 LLM 自评 | Task 4 (规则匹配,无自评) |

### 2. Placeholder scan

- 无 "TBD" / "TODO" / "implement later"
- 每个步骤都有完整代码
- 测试代码可直接运行

### 3. Type consistency

- `LessonAction` 在 Task 2 定义,Task 4/5/6 使用,字段名一致(condition/action/signal/confidence)
- `FailureSignal` 在 Task 3 定义为 `(String, String)`,Task 4/6 使用一致
- `LessonsConfig` 字段(interval_turns/lookback_turns/max_entries_per_distill)跨 Task 一致
- `MAX_LESSONS_ENTRIES` / `INITIAL_CONFIDENCE` / `MAX_CONFIDENCE` 常量定义后跨 Task 引用一致

---

## 执行风险与回滚

### 风险

1. **ConversationMessage / ContentBlock 字段不匹配** — Task 4/6 的测试代码假设了字段名,实际可能不同。回滚:按实际定义调整。
2. **NOTEBOOK_MAX_CHARS 超限** — lessons 段最多 20 条,每条约 100 chars,共 2000 chars,远低于 16000 上限。风险低。
3. **性能影响** — 蒸馏每 5 轮触发一次,扫描最近 15 条消息,O(n) 复杂度,可忽略。
4. **poor_mode 短路** — 已在 `distill_lessons` 入口判断,穷鬼模式不触发。

### 回滚

每个 Task 独立 commit,可按需 `git revert`。最坏情况删除 `lessons_distiller.rs` 并回退 `notebook.rs` / `conversation.rs` / `lib.rs` 的修改。

---

## Phase 2 预留(不在本 Plan 范围)

- **失败聚类**:复用 `trace_analyzer::cluster_failures_kmeans`,按 (failure_kind, error_embedding) 聚类
- **保守晋升**:lesson 在 ≥2 次独立会话复现后,升级为 DecisionLog 条目(success_rate 学习环)
- **simhash 去重**:启用 `decision_log.rs` 已实现但未使用的 `hamming_distance` 做模糊去重
- **LLM 辅助提取**:在规则式基础上,可选启用 LLM 提取更复杂 lesson(需配合外部验证信号)

---

## Execution Handoff

Plan complete and saved to `docs/plans/2026-07-24-p3-self-evolving-lessons.md`. Two execution options:

1. **Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration
2. **Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
