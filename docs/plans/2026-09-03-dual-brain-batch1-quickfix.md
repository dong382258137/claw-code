# 双大脑 v5.1 · 批次 1(快修)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 superpowers:subagent-driven-development(推荐)或 superpowers:executing-plans 逐任务实施本计划。步骤用 checkbox(`- [ ]`)追踪。

**Goal:** 修复 ask_brain 管理脑会话被 ReadOnly 迭代上限截断的问题(机制 3),并为会话 digest 增加写端重要性门控去噪(机制 4 写端),两者均不引入新进程,只在 runtime 内改动。

**Architecture:** 机制 3 通过给 `run_subagent_turn_with_model` 增加可选 `max_iterations` 参数(经 `ctx.max_iterations` 覆盖 ReadOnly 默认 5),使 ask_brain 综合会话放宽到 10 轮并约束"先综合后核实";机制 4 将会话结束的内联 digest 聚合逻辑抽成可单测的纯函数,新增重要性门控——噪音会话(无重复失败)不产出 digest 或产出带"(本会话无工具失败)"的空骨架,重复失败必记录。

**Tech Stack:** Rust(runtime crate)、tokio 测试、tempfile。工作目录 `d:\claw-code-src\rust`。

**Spec:** `docs/2026-09-03-dual-brain-optimization-design.md` §4.5(机制 3)、§4.6 写端(机制 4)、§11 批次 1。

---

## 文件结构

| 文件 | 职责 | 改动 |
|---|---|---|
| `crates/runtime/src/conversation.rs` | 主会话运行时;含 `execute_ask_brain_async`(7616)、`run_subagent_turn_with_model`(6018)、会话结束 digest 聚合(5218-5262) | Modify |
| `crates/runtime/src/management_brain.rs` | 管理脑 digest 纯函数与落盘 | Modify(新增门控纯函数) |
| 测试 | 均在上述文件 `#[cfg(test)]` 内(项目惯例,与既有 ask_brain 测试同处) | Modify |

> 代码引用基于当前工作区行号(2026-09-03,含在途 dag max_iterations 改造已收尾,cargo check 通过)。实施前若行号漂移,以符号名定位。

---

### Task 1: `run_subagent_turn_with_model` 支持可选 `max_iterations`

**Files:**
- Modify: `crates/runtime/src/conversation.rs:6018`(`async fn run_subagent_turn_with_model`)

- [ ] **Step 1: 给函数签名加 `max_iterations: Option<usize>` 参数**

在 [conversation.rs:6018](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L6018) 处,将:

```rust
    async fn run_subagent_turn_with_model(
        &mut self,
        subagent_id: &str,
        name: &str,
        task: &str,
        workspace_override: Option<&std::path::Path>,
        model: Option<&str>,
        complexity: crate::multi_agent::TaskComplexity,
        capability: crate::multi_agent::SubagentCapability,
    ) -> Result<String, String> {
```

改为:

```rust
    async fn run_subagent_turn_with_model(
        &mut self,
        subagent_id: &str,
        name: &str,
        task: &str,
        workspace_override: Option<&std::path::Path>,
        model: Option<&str>,
        max_iterations: Option<usize>,
        complexity: crate::multi_agent::TaskComplexity,
        capability: crate::multi_agent::SubagentCapability,
    ) -> Result<String, String> {
```

- [ ] **Step 2: 在 `build_subagent_context` 之后注入 `ctx.max_iterations`**

在 [conversation.rs:6062](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L6062)(`let ctx = self.build_subagent_context(capability, workspace_override.as_deref());`)之后,插入:

```rust
        let mut ctx = ctx;
        if max_iterations.is_some() {
            ctx.max_iterations = max_iterations;
        }
```

> 注:不能无条件覆盖——调用方传 `None` 时必须保持 `ctx` 原值(该 ctx 可能已被 build_subagent_context 依任务注入过 max_iterations,见 SubagentContext 注释)。仅当显式传入 `Some(n)` 才覆盖。

- [ ] **Step 3: 编译检查**

Run: `cargo check -p runtime 2>&1 | Select-Object -First 15`
Expected: 编译错误提示所有调用点缺参(compile error: missing argument)——这是预期,下一步修复调用点。

- [ ] **Step 4: 修复全部调用点(补传 `None`)**

搜索所有 `run_subagent_turn_with_model(` 调用点并补一个 `None` 参数(位置在 `model` 之后)。用 Grep 定位:

```text
pattern: run_subagent_turn_with_model\(
path: crates/runtime/src
```

预期 2-3 处调用点(ask_brain 内部 + 可能的多 agent 派发)。每处在 `None,`(model 参数)后再加一行 `None,`。示例:

```rust
        match self
            .run_subagent_turn_with_model(
                &subagent_id,
                "management-brain",
                &task,
                None,
                None,
                None, // max_iterations: ask_brain 默认能力上限(批次 1 Task 3 会传 Some)
                crate::multi_agent::TaskComplexity::Simple,
                crate::multi_agent::SubagentCapability::ReadOnly,
            )
            .await
```

- [ ] **Step 5: 编译检查通过**

Run: `cargo check -p runtime 2>&1 | Select-Object -First 15`
Expected: `Finished dev profile`(无错误)。

- [ ] **Step 6: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "feat(dual-brain): run_subagent_turn_with_model 支持可选 max_iterations 覆盖"
```

---

### Task 2: ask_brain 传入放宽的迭代上限 + 任务文本约束

**Files:**
- Modify: `crates/runtime/src/conversation.rs:7616`(`execute_ask_brain_async`)

- [ ] **Step 1: 写失败测试(断言 ask_brain 传迭代上限)**

在 conversation.rs 测试区(现有 `execute_ask_brain_poor_mode_skips_brain_synthesis` 之后,约 [conversation.rs:19345](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L19345))新增测试。此测试用自定义 `RecordingApi` 捕获 ask_brain 发起的 subagent 请求,断言其 messages 中携带"先综合后核实"约束(证明 max_iterations 放宽的 task 文本已生效)。若 `execute_ask_brain_async` 尚未放宽,则 task 文本不含该约束 → 测试红。

```rust
    /// 机制 3(批次 1):ask_brain 的 task 文本必须携带"先综合后核实,最多核实
    /// 2 个文件"的轮次预算约束,防止 brain 会话把 5 轮耗在文件侦察上。
    /// 用 RecordingApi 捕获 subagent 请求文本验证(不真调 LLM)。
    #[tokio::test]
    async fn ask_brain_task_contains_iteration_budget_constraint() {
        struct RecordingApi {
            captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        }
        impl ApiClient for RecordingApi {
            fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                // 捕获最后一条 user 消息(即 subagent task)
                for m in &request.messages {
                    if let Some(text) = m.text_content() {
                        self.captured.lock().unwrap().push(text.to_string());
                    }
                }
                // 返回一个空 assistant 事件,让 subagent 单轮即收尾
                Ok(vec![])
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let log = crate::decision_log::DecisionLog::open(dir.path()).unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let api = RecordingApi {
            captured: captured.clone(),
        };
        let mut runtime: ConversationRuntime<RecordingApi, StaticToolExecutor> =
            ConversationRuntime::new(
                Session::new(),
                api,
                StaticToolExecutor::new(),
                PermissionPolicy::new(PermissionMode::DangerFullAccess),
                vec!["system".to_string()],
            )
            .with_decision_log(log)
            .with_workspace_root(dir.path().to_path_buf());
        // 先注入一条决策,使检索非空
        runtime
            .execute_log_decision(
                r#"{"session_id":"s1","problem_signature":"edit_file: old_string not found","root_cause_hypothesis":"stale content","applied_solution":"re-read file before edit","affected_files":[],"verification_result":"Confirmed"}"#,
            )
            .expect("log_decision");

        runtime
            .execute_ask_brain_async(r#"{"query":"old_string not found"}"#)
            .await
            .expect("ask_brain");

        let all: Vec<String> = captured.lock().unwrap().clone();
        assert!(
            all.iter().any(|t| t.contains("先综合后核实")),
            "task 缺轮次预算约束,捕获文本: {all:?}"
        );
    }
```

- [ ] **Step 2: 运行测试验证红**

Run: `cargo test -p runtime ask_brain_task_contains_iteration_budget_constraint 2>&1 | Select-Object -Last 10`
Expected: FAIL——当前 task 文本无"先综合后核实"约束。

- [ ] **Step 3: 修改 ask_brain task 文本与调用**

在 [conversation.rs:7666](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L7666) 的 task 构建中,把任务约束第 2 条改为带预算:

```rust
        let task = format!(
            "# Management Brain Consultation\n\n\
             ## 执行脑的问题\n{query}\n\n\
             ## 执行脑当前上下文\n{context}\n\n\
             ## 知识库检索片段(来源标注,供你综合与核实)\n{trimmed}\n\n\
             ## 你的任务\n\
             1. 基于上述检索片段(仅作为参考,不代表全部事实),回答执行脑的问题;\n\
             2. 轮次预算:先综合后核实。最多核实 2 个文件(优先读检索片段已引用的),\n\
                不要逐文件侦察——本会话总轮次上限已放宽到 10,但请尽快产出结论;\n\
             3. 输出格式:先给结论,再给依据。每条关键结论后附来源(如:decision_log 条目 / 会话历史 / 文件路径:行号);\n\
             4. 若检索片段不足以回答,明确说明\"知识库无足够证据\",并给出基于代码事实的最佳建议;\n\
             5. 你是管理脑(咨询角色):只提议不直改。执行脑会自行决定是否采纳,并以行动结果(编译/测试)为准。"
        );
```

并在调用处([conversation.rs:7686](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L7686))传 `Some(10)`:

```rust
        match self
            .run_subagent_turn_with_model(
                &subagent_id,
                "management-brain",
                &task,
                None,
                None,
                Some(10), // max_iterations:brain 综合放宽到 10 轮(ReadOnly 默认 5 会在文件侦察上耗尽)
                crate::multi_agent::TaskComplexity::Simple,
                crate::multi_agent::SubagentCapability::ReadOnly,
            )
            .await
```

- [ ] **Step 4: 运行测试验证绿**

Run: `cargo test -p runtime ask_brain_task_contains_iteration_budget_constraint 2>&1 | Select-Object -Last 10`
Expected: PASS。

- [ ] **Step 5: 回归既有 ask_brain 测试**

Run: `cargo test -p runtime ask_brain 2>&1 | Select-Object -Last 12`
Expected: 全部 PASS(empty_knowledge / degrades_without_workspace_root / poor_mode / run_turn_intercepts 均不受影响)。

- [ ] **Step 6: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "feat(dual-brain): ask_brain 放宽 brain 会话至 10 轮并约束先综合后核实(机制3批次1)"
```

---

### Task 3: 结构性兜底——超轮时返回部分结论而非错误串

**Files:**
- Modify: `crates/runtime/src/conversation.rs:7698`(ask_brain 的 Err 分支)

- [ ] **Step 1: 理解现状(无需先写测试——纯文案降级,断言价值低,但需验证行为)**

现状 [conversation.rs:7698-7702](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L7698-L7702):brain 会话超轮返回 `Err("subagent exceeded max_iterations ...")`,被降级为"brain 会话暂不可用 + 检索片段"。问题:执行脑看不到 brain 已产出的部分结论(该 Err 文本含 `.claw/subagents/<id>.md` 部分结果路径,但被错误文案掩盖)。

- [ ] **Step 2: 改写 Err 分支,提示部分结论位置**

将 [conversation.rs:7698-7702](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L7698-L7702) 改为:

```rust
            Ok(result) => Ok(result),
            Err(e) => {
                // 机制 3 结构性兜底:brain 会话超轮或失败时,若已产出部分结论
                // (handoff 落盘于 .claw/subagents/brain-*.md),提示执行脑可去读取;
                // 同时附检索片段保证不阻塞。非"暂不可用"——超轮≠不可用。
                let hint = if e.contains("max_iterations") {
                    format!(
                        "ask_brain: brain 会话达到轮次上限,部分结论已落盘 \
                         (见 .claw/subagents/{subagent_id}.md,含 summary/details)。\n\
                         可直接读取该文件获取已核实结论;以下为检索片段(供参考):\n\n{trimmed}"
                    )
                } else {
                    format!(
                        "ask_brain: brain 会话执行失败({e})。以下为确定性检索片段(未综合,可直接参考):\n\n{trimmed}"
                    )
                };
                Ok(hint)
            }
```

- [ ] **Step 3: 编译 + 既有测试回归**

Run: `cargo test -p runtime ask_brain 2>&1 | Select-Object -Last 12`
Expected: 全部 PASS(现有 degrades 测试断言"brain 会话不可用"的是**无 workspace_root** 分支,不经过此 Err 路径,不受影响)。

- [ ] **Step 4: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "feat(dual-brain): ask_brain 超轮时提示部分结论落盘位置(机制3结构性兜底)"
```

---

### Task 4: 抽出 digest 失败聚合纯函数(写端门控前置)

**Files:**
- Modify: `crates/runtime/src/management_brain.rs`(新增纯函数)
- Modify: `crates/runtime/src/conversation.rs:5218-5262`(调用纯函数)

- [ ] **Step 1: 在 management_brain.rs 新增聚合纯函数**

在 [management_brain.rs:26](file:///d:/claw-code-src/rust/crates/runtime/src/management_brain.rs#L26)(`render_digest` 之前)新增:

```rust
/// 从会话失败记录中按"重复失败"门控过滤,返回应写入 digest 的失败列表。
///
/// 写端重要性门控(v5.1 §4.6):只记录**重复失败**(同工具同签名出现 ≥2 次),
/// 单次偶发失败视为噪音跳过。判定用精确签名——同工具且首行签名一致
/// (当前调用方在聚合前已 sort+dedup,故此处输入应为去重后列表,无法再判断
/// 原始重复次数;实际重复判定由调用方在 dedup 前统计——见 conversation.rs
/// 调用处说明)。
///
/// 纯函数,便于单测。
pub fn filter_notable_failures(failures: &[ToolFailure]) -> Vec<ToolFailure> {
    // 注:调用方已 sort+dedup,此处对已去重列表无法还原重复次数。
    // 门控语义由调用方在聚合前统计(见 Task 5),本函数保留为:
    // - 空输入 → 空输出
    // - 非空 → 保留全部(去重后的每条都代表一类失败,值得记录)
    failures.to_vec()
}
```

> ⚠️ 设计说明:当前 conversation.rs 5218-5262 在扫描时**先 collect 全部失败再 sort+dedup**,重复计数信息在 dedup 时丢失。写端门控的正确实现需要**在 dedup 前统计每 (tool, sig) 出现次数**。Task 4 先把聚合抽纯、Task 5 把去重前移并实现真正的门控。若直接在此处实现门控则拿不到重复计数——故本 Task 的 `filter_notable_failures` 是**过渡纯函数**,真正的门控在 Task 5 用新的 `count_failure_occurrences` 实现。

- [ ] **Step 2: 新增纯函数单测**

在 management_brain.rs 测试区新增:

```rust
    #[test]
    fn filter_notable_failures_keeps_all_after_dedup() {
        let f = vec![
            ("edit_file".to_string(), "old_string not found".to_string()),
            ("bash".to_string(), "command not found".to_string()),
        ];
        let out = filter_notable_failures(&f);
        assert_eq!(out.len(), 2, "去重后每条失败均值得记录");
    }

    #[test]
    fn filter_notable_failures_empty_input() {
        assert!(filter_notable_failures(&[]).is_empty());
    }
```

- [ ] **Step 3: 运行测试**

Run: `cargo test -p runtime filter_notable_failures 2>&1 | Select-Object -Last 10`
Expected: 2 个测试 PASS。

- [ ] **Step 4: Commit**

```bash
git add rust/crates/runtime/src/management_brain.rs
git commit -m "refactor(dual-brain): 抽出 digest 失败过滤纯函数(写端门控前置)"
```

---

### Task 5: 会话结束 digest 写端门控(重复失败才记录)

**Files:**
- Modify: `crates/runtime/src/conversation.rs:5218-5262`(聚合逻辑)

- [ ] **Step 1: 理解当前逻辑缺陷**

[conversation.rs:5218-5262](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L5218-L5262) 现状:
1. 扫描 messages 收集所有 is_error 的 (tool_name, sig);
2. `failures.sort(); failures.dedup();` —— **重复计数在此丢失**;
3. render_digest + write_digest。

写端门控目标(v5.1 §4.6):**单次偶发失败 = 噪音,不应出现在 digest**;重复失败(同工具同签名 ≥2 次)才是值得跨会话感知的模式。

- [ ] **Step 2: 写失败测试**

在 conversation.rs 测试区新增。测试构造一个会话:包含 2 次相同失败 + 1 次偶发失败,断言 digest 只记录重复失败。当前实现不区分 → 会记录全部 → 测试红。

```rust
    /// 机制 4 写端门控(批次 1):digest 只记录重复失败(同签名≥2),单次偶发失败视为噪音。
    #[test]
    fn digest_gate_keeps_only_repeated_failures() {
        // 用两个相同签名 + 一个单次签名,验证门控过滤
        let repeated = (
            "edit_file".to_string(),
            "old_string not found in file".to_string(),
        );
        let single = ("bash".to_string(), "command not found".to_string());
        let raw = vec![repeated.clone(), repeated.clone(), single.clone()];

        // 模拟当前聚合:sort + dedup(现状在 dedup 前丢失计数)
        let mut deduped = raw.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), 2, "现状去重后 2 类");

        // 门控:统计原始出现次数,只保留 ≥2 的
        use std::collections::BTreeMap;
        let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
        for f in &raw {
            *counts.entry(f.clone()).or_insert(0) += 1;
        }
        let notable: Vec<_> = counts
            .iter()
            .filter(|(_, c)| **c >= 2)
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(notable.len(), 1, "只有重复失败通过门控");
        assert_eq!(notable[0], repeated, "保留的是重复失败而非单次失败");
    }
```

> 该测试在 conversation.rs 测试区验证门控语义;实际落盘路径的接线在 Step 3-4。

- [ ] **Step 3: 运行测试验证红**

Run: `cargo test -p runtime digest_gate_keeps_only_repeated_failures 2>&1 | Select-Object -Last 10`
Expected: PASS(注意:此测试当前实现"恰好通过"因为它直接验证门控逻辑,不经过聚合路径。若 Step 2 测试未红,说明测试写的是纯逻辑而非现状差异——需在 Step 4 实现后加**端到端**断言:构造含重复失败的 session.messages,跑聚合路径,断言 digest 文本不含单次失败)。

- [ ] **Step 4: 改造聚合路径——去重前统计次数,门控后 render**

将 [conversation.rs:5221-5262](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L5221-L5262) 整段替换为:

```rust
        if let Some(workspace_root) = &self.workspace_root {
            // 1. 聚合失败记录(写端门控 v5.1 §4.6):统计 (tool, 首行签名)
            //    原始出现次数,只保留重复失败(≥2),单次偶发失败视为噪音。
            //    去重前计数——不能在 dedup 后统计(计数信息会丢失)。
            use std::collections::BTreeMap;
            let mut raw_failures: Vec<(String, String)> = Vec::new();
            for msg in &self.session.messages {
                for block in &msg.blocks {
                    if let crate::session::ContentBlock::ToolResult {
                        tool_name,
                        output,
                        is_error: true,
                        ..
                    } = block
                    {
                        let first_line = output.lines().next().unwrap_or("").trim();
                        let sig = if first_line.is_empty() {
                            output.chars().take(100).collect()
                        } else {
                            first_line.chars().take(100).collect()
                        };
                        raw_failures.push((tool_name.clone(), sig));
                    }
                }
            }
            // 门控:同 (tool, sig) 出现 ≥2 次才进 digest;稳定排序保证确定性。
            let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
            for f in &raw_failures {
                *counts.entry(f.clone()).or_insert(0) += 1;
            }
            let notable: Vec<(String, String)> = counts
                .iter()
                .filter(|(_, c)| **c >= 2)
                .map(|(k, _)| k.clone())
                .collect();
            // 2. 用量成本行。
            let usage_lines = self
                .usage_tracker
                .cumulative_usage()
                .summary_lines("session-total");
            // 3. 渲染 + 落盘。
            let digest = crate::management_brain::render_digest(
                &self.session.session_id,
                &notable,
                &usage_lines,
            );
            let _ = crate::management_brain::write_digest(
                workspace_root,
                &self.session.session_id,
                &digest,
            );
        }
```

- [ ] **Step 5: 新增端到端断言(聚合路径真实门控)**

在 conversation.rs 测试区新增(验证真实聚合路径,而非纯逻辑):

```rust
    /// 端到端:真实聚合路径只把重复失败写入 digest,单次失败被门控滤除。
    #[tokio::test]
    async fn session_end_digest_filters_single_failures() {
        // 构造一个带 2 次同签名失败 + 1 次单次失败的会话,跑 run_turn 结束路径,
        // 读取落盘 digest 断言只含重复失败。
        let dir = tempfile::tempdir().unwrap();
        let mut runtime: ConversationRuntime<NoopApi, StaticToolExecutor> =
            ConversationRuntime::new(
                Session::new(),
                NoopApi,
                StaticToolExecutor::new(),
                PermissionPolicy::new(PermissionMode::DangerFullAccess),
                vec!["system".to_string()],
            )
            .with_workspace_root(dir.path().to_path_buf());

        // 注入两条同签名失败 ToolResult + 一条单次失败(直接构造消息块,绕过 LLM)
        let mk_err = |tool: &str, sig: &str| crate::session::ConversationMessage {
            role: crate::session::Role::Assistant,
            blocks: vec![crate::session::ContentBlock::ToolResult {
                tool_use_id: format!("call-{}", std::time::SystemTime::now().elapsed().unwrap().as_nanos()),
                tool_name: tool.to_string(),
                input: String::new(),
                output: format!("{sig}\nmore detail"),
                is_error: true,
                duration_ms: 0,
            }],
            ts_ms: 0,
            model: None,
        };
        let _ = runtime.session.push_user_text("go".to_string());
        // 直接调用会触发 digest 的收尾方法(若 run_turn 需 LLM,改用测试专用收尾钩子)
        // 注:此处通过 run_turn 会走 NoopApi(返回空)难以构造失败;更可靠的是
        // 直接调用聚合段所在的方法。若聚合逻辑未抽成独立方法,此端到端测试
        // 改为验证 render_digest 输入——见 Task 6 抽方法后的补测。
        eprintln!("digest gate e2e: 依赖 Task 6 抽方法后完善");
    }
```

> ⚠️ 诚实标注:此端到端测试因 digest 聚合内联在 run_turn 收尾、难以在不跑完整 LLM turn 的情况下注入失败消息,当前为骨架。**Task 6 会先把聚合段抽成独立方法**,届时此测试补全为真断言。

- [ ] **Step 6: 编译 + 运行门控相关测试**

Run: `cargo test -p runtime digest_gate_keeps_only_repeated_failures 2>&1 | Select-Object -Last 10`
Expected: PASS(门控语义测试)。

Run: `cargo test -p runtime render_digest 2>&1 | Select-Object -Last 10`
Expected: PASS(render_digest 既有测试不受影响——空失败仍渲染"(本会话无工具失败)")。

- [ ] **Step 7: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs rust/crates/runtime/src/management_brain.rs
git commit -m "feat(dual-brain): digest 写端门控——仅重复失败(≥2)写入,单次噪音滤除(机制4批次1)"
```

---

### Task 6: 抽取会话收尾 digest 方法并补全端到端测试

**Files:**
- Modify: `crates/runtime/src/conversation.rs`(抽出 `write_session_digest_if_relevant` 方法)

- [ ] **Step 1: 抽出独立方法**

把 Task 5 Step 4 的聚合逻辑(从 `if let Some(workspace_root) = &self.workspace_root {` 到 write_digest 结束)抽成 runtime 的私有方法:

```rust
    /// 双大脑 · 会话结束写端门控 digest(机制 4,批次 1)。
    ///
    /// 扫描会话 ToolResult.is_error,按 (tool, 首行签名) 统计出现次数,
    /// 仅重复失败(≥2)写入 digest(单次偶发失败 = 噪音)。零 LLM;失败静默。
    /// 抽为独立方法,便于端到端单测(不必跑完整 LLM turn)。
    fn write_session_digest_if_relevant(&self) {
        let Some(workspace_root) = &self.workspace_root else {
            return;
        };
        use std::collections::BTreeMap;
        let mut raw_failures: Vec<(String, String)> = Vec::new();
        for msg in &self.session.messages {
            for block in &msg.blocks {
                if let crate::session::ContentBlock::ToolResult {
                    tool_name,
                    output,
                    is_error: true,
                    ..
                } = block
                {
                    let first_line = output.lines().next().unwrap_or("").trim();
                    let sig = if first_line.is_empty() {
                        output.chars().take(100).collect()
                    } else {
                        first_line.chars().take(100).collect()
                    };
                    raw_failures.push((tool_name.clone(), sig));
                }
            }
        }
        let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
        for f in &raw_failures {
            *counts.entry(f.clone()).or_insert(0) += 1;
        }
        let notable: Vec<(String, String)> = counts
            .iter()
            .filter(|(_, c)| **c >= 2)
            .map(|(k, _)| k.clone())
            .collect();
        let usage_lines = self
            .usage_tracker
            .cumulative_usage()
            .summary_lines("session-total");
        let digest =
            crate::management_brain::render_digest(&self.session.session_id, &notable, &usage_lines);
        let _ = crate::management_brain::write_digest(workspace_root, &self.session.session_id, &digest);
    }
```

然后在原收尾处([conversation.rs:5218 注释区])替换为:

```rust
        // 双大脑架构(批次 3 + 批次 1 门控):会话结束确定性聚合 digest → 落盘。
        // 见 write_session_digest_if_relevant(机制 4 写端门控:仅重复失败写入)。
        self.write_session_digest_if_relevant();
```

- [ ] **Step 2: 补全端到端测试**

将 Task 5 Step 5 的骨架测试补全为真断言(现可直调 `write_session_digest_if_relevant`):

```rust
    /// 端到端:write_session_digest_if_relevant 只把重复失败写入 digest。
    #[tokio::test]
    async fn session_end_digest_filters_single_failures() {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime: ConversationRuntime<NoopApi, StaticToolExecutor> =
            ConversationRuntime::new(
                Session::new(),
                NoopApi,
                StaticToolExecutor::new(),
                PermissionPolicy::new(PermissionMode::DangerFullAccess),
                vec!["system".to_string()],
            )
            .with_workspace_root(dir.path().to_path_buf());

        // 构造 3 条 ToolResult:2 条同签名失败(edit_file) + 1 条单次失败(bash)
        let mut push_err = |runtime: &mut ConversationRuntime<NoopApi, StaticToolExecutor>,
                            tool: &str,
                            sig: &str| {
            let msg = crate::session::ConversationMessage {
                role: crate::session::Role::Assistant,
                blocks: vec![crate::session::ContentBlock::ToolResult {
                    tool_use_id: format!("call-{}", std::time::SystemTime::now().elapsed().unwrap().as_nanos()),
                    tool_name: tool.to_string(),
                    input: String::new(),
                    output: format!("{sig}\nmore detail"),
                    is_error: true,
                    duration_ms: 0,
                }],
                ts_ms: 0,
                model: None,
            };
            runtime.session.messages.push(msg);
        };
        push_err(&mut runtime, "edit_file", "old_string not found");
        push_err(&mut runtime, "edit_file", "old_string not found");
        push_err(&mut runtime, "bash", "command not found");

        runtime.write_session_digest_if_relevant();

        // 读取落盘 digest
        let digest_dir = dir.path().join(".claw/management_brain");
        let files: Vec<_> = std::fs::read_dir(&digest_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(files.len(), 1, "应产出 1 份 digest");
        let text = std::fs::read_to_string(&files[0]).unwrap();
        assert!(text.contains("edit_file"), "digest 应含重复失败: {text}");
        assert!(
            !text.contains("command not found"),
            "digest 不应含单次失败: {text}"
        );
    }
```

- [ ] **Step 3: 运行端到端测试**

Run: `cargo test -p runtime session_end_digest_filters_single_failures 2>&1 | Select-Object -Last 12`
Expected: PASS。

- [ ] **Step 4: 全量回归(runtime digest + ask_brain)**

Run: `cargo test -p runtime management_brain 2>&1 | Select-Object -Last 8`
Expected: PASS。

Run: `cargo test -p runtime ask_brain 2>&1 | Select-Object -Last 8`
Expected: PASS。

Run: `cargo check -p runtime 2>&1 | Select-Object -First 5`
Expected: `Finished` 无错误。

- [ ] **Step 5: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "test(dual-brain): 抽取 write_session_digest_if_relevant 并补端到端门控测试"
```

---

### Task 7: clippy + 收尾(批次 1 验收)

**Files:**
- 无新增;全量验证

- [ ] **Step 1: clippy 全 workspace**

Run: `cargo clippy -p runtime --all-targets -- -D warnings 2>&1 | Select-Object -First 15`
Expected: 无 error 无 warning(新增代码若触发 clippy,修复后重跑)。

- [ ] **Step 2: 相关单测全量**

Run: `cargo test -p runtime management_brain 2>&1 | Select-Object -Last 6`
Expected: PASS。

Run: `cargo test -p runtime ask_brain 2>&1 | Select-Object -Last 6`
Expected: PASS。

Run: `cargo test -p runtime digest_gate 2>&1 | Select-Object -Last 6`
Expected: PASS。

Run: `cargo test -p runtime session_end_digest 2>&1 | Select-Object -Last 6`
Expected: PASS。

- [ ] **Step 3: 提交版本号更新(项目约定)**

提交前按项目约定更新 `rust/Cargo.toml` workspace version 为当天日期,随后 `cargo check` 刷新 Cargo.lock 并同一 commit 提交。

Run: `git status --short | Select-String "Cargo.toml"`
Expected: 确认 Cargo.toml 版本已更新(如需,按 project_memory 约定执行)。

- [ ] **Step 4: 最终 Commit(若 Step 3 有版本变更)**

```bash
git add rust/Cargo.toml rust/Cargo.lock
git commit -m "chore: bump version"
```

---

## Self-Review

**1. Spec coverage:**
- 机制 3(§4.5):Task 1(max_iterations 参数)+ Task 2(ask_brain 传 Some(10) + 任务文本约束)+ Task 3(超轮部分结论提示)✅
- 机制 4 写端(§4.6):Task 4(纯函数前置)+ Task 5(去重前计数 + 门控)+ Task 6(抽方法 + 端到端)✅
- 批次 1 范围(§11):brain 截断修复 + digest 写端门控 ✅ 无越界(未做 DigestRouter 读端/守护进程——属批次 2+)

**2. Placeholder scan:** 无 TBD/TODO;Task 5 Step 5 的骨架测试在 Task 6 Step 2 明确补全(诚实标注依赖关系)。

**3. Type consistency:**
- `run_subagent_turn_with_model` 新参数 `max_iterations: Option<usize>` 插入位置(model 之后)在 Task 1/2 一致;
- `ctx.max_iterations: Option<usize>`(SubagentContext 字段)与 Task 1 Step 2 注入一致;
- `write_session_digest_if_relevant(&self)` 在 Task 6 Step 1 定义、Step 2 直调一致;
- `filter_notable_failures(&[ToolFailure]) -> Vec<ToolFailure>` 在 Task 4 定义与测试一致(Task 5 实际用 BTreeMap 计数而非该函数——已在 Task 4 Step 1 的 ⚠️ 注明该函数为过渡,避免误导;若实施中确认 filter_notable_failures 无用,删除之并只保留 Task 5 内联计数,保持最小改动)。

**遗留(不在批次 1,属批次 2/3):** DigestRouter 读端相关度检索、守护进程、EvidenceTracker、injection 通道——均在后续批次,本计划不触碰。
