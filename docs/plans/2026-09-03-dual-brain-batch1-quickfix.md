# 双大脑 · 批次 1(裁剪版)实施计划 —— 仅 ask_brain 修复

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 superpowers:subagent-driven-development(推荐)或 superpowers:executing-plans 逐任务实施本计划。步骤用 checkbox(`- [ ]`)追踪。

**Goal:** 修复 ask_brain 管理脑会话被 ReadOnly 迭代上限截断的问题(机制 3)。**裁剪决策(2026-09-03):digest 写端门控与全部后续批次暂缓,不在本计划内。**

**Architecture:** 给 `run_subagent_turn_with_model` 增加可选 `max_iterations` 参数(经 `ctx.max_iterations` 覆盖 ReadOnly 默认 5),使 ask_brain 综合会话放宽到 10 轮;task 文本加"先综合后核实"预算约束;超轮时提示部分结论落盘位置而非误导性"暂不可用"。全程 runtime 内改动,不引入新进程。

**Tech Stack:** Rust(runtime crate)、tokio 测试。工作目录 `d:\claw-code-src\rust`。

**Spec:** `docs/2026-09-03-dual-brain-optimization-design.md` §4.5(机制 3)+ §11 批次 1(裁剪后)。

---

## 裁剪决策记录(为何只做 ask_brain 修复)

原批次 1 = ask_brain 修复(机制 3)+ digest 写端门控(机制 4 写端)。经重新评估(证据驱动)裁剪:

- **做 ask_brain 修复**:执行脑真实可感知的缺陷(实测 brain 会话 5 轮耗尽返回误导性"暂不可用"),改动小、成本几乎无增、消除误导。✅
- **暂缓 digest 写端门控**:digest 的**读侧从未实证被消费**(management_brain/ 仅 1 份 digest,新会话注入是否改变行为未知)。优化写端 = 优化一条可能没人读的管道(下游未验证就优化上游)。待读侧实证后恢复。
- **暂缓批次 2-4(守护进程/投喂/验证环复活/自治升级)**:决策库仅 15 条、verify_count 全 0、ask_brain 检索命中率近空。投喂收益 = 库质量 × 匹配准确率,两因子当前近 0;常驻进程与注入是确定性成本。属"为空库盖楼"。**恢复条件见下。**

**恢复条件(知识库自然积累优先):** 真实任务中 `log_decision` + 用户显式确认持续积累,当决策库出现 ≥N 条用户确认的高质量条目(建议 N=20,数值待定)后,重新评估投喂/守护进程的收益成本比。任何后续批次开工前须先实证 digest 读侧与 ask_brain 在真实任务中的价值。

---

## 文件结构

| 文件 | 职责 | 改动 |
|---|---|---|
| `crates/runtime/src/conversation.rs` | 主会话运行时;含 `execute_ask_brain_async`、`run_subagent_turn_with_model` | Modify(仅此一文件) |

> 代码引用基于当前工作区符号名(行号可能随在途改动漂移,以符号名定位为准)。

---

### Task 1: `run_subagent_turn_with_model` 支持可选 `max_iterations`

**Files:**
- Modify: `crates/runtime/src/conversation.rs`(`async fn run_subagent_turn_with_model`,当前约 6018 行)

- [ ] **Step 1: 给函数签名加 `max_iterations: Option<usize>` 参数**

将:

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

在 `let ctx = self.build_subagent_context(capability, workspace_override.as_deref());` 之后,插入:

```rust
        let mut ctx = ctx;
        if max_iterations.is_some() {
            ctx.max_iterations = max_iterations;
        }
```

> 注:不能无条件覆盖——调用方传 `None` 时必须保持 `ctx` 原值(该 ctx 可能已被 build_subagent_context 依任务注入过 max_iterations)。仅当显式传入 `Some(n)` 才覆盖。

- [ ] **Step 3: 编译检查(预期报缺参错误,是正常的)**

Run: `cargo check -p runtime 2>&1 | Select-Object -First 15`
Expected: 编译错误提示所有调用点缺参(missing argument)——预期,下一步修复调用点。

- [ ] **Step 4: 修复全部调用点(补传 `None`)**

用 Grep 定位所有 `run_subagent_turn_with_model(` 调用点(预期 2-3 处,含 ask_brain 内部),每处在 `model` 参数(`None,`)后再加一行 `None,`(max_iterations)。示例:

```rust
        match self
            .run_subagent_turn_with_model(
                &subagent_id,
                "management-brain",
                &task,
                None,
                None,
                None, // max_iterations:本调用点默认能力上限
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

### Task 2: ask_brain 传入放宽的迭代上限 + 任务文本预算约束

**Files:**
- Modify: `crates/runtime/src/conversation.rs`(`execute_ask_brain_async`,当前约 7616 行)

- [ ] **Step 1: 写失败测试(断言 ask_brain task 文本含预算约束)**

在 conversation.rs 测试区(现有 `execute_ask_brain_poor_mode_skips_brain_synthesis` 之后)新增。用 `RecordingApi` 捕获 ask_brain 发起的 subagent 请求文本,断言含"先综合后核实"。未实现时不含该约束 → 测试红。

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
                for m in &request.messages {
                    if let Some(text) = m.text_content() {
                        self.captured.lock().unwrap().push(text.to_string());
                    }
                }
                Ok(vec![]) // 空 assistant 事件,subagent 单轮即收尾
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
        // 注入一条决策使检索非空(否则走"无相关知识"早退路径,不会调 brain)
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

> 注:`m.text_content()` 若不存在,改用 `m.blocks` 遍历提取 `ContentBlock::Text`(以实际 Session API 为准)。

- [ ] **Step 2: 运行测试验证红**

Run: `cargo test -p runtime ask_brain_task_contains_iteration_budget_constraint 2>&1 | Select-Object -Last 10`
Expected: FAIL——当前 task 文本无"先综合后核实"约束。

- [ ] **Step 3: 修改 ask_brain task 文本与调用**

在 task 构建处,把约束第 2 条改为带预算:

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

并在 `run_subagent_turn_with_model(...)` 调用处,把 Task 1 加的 `None,`(max_iterations)改为 `Some(10),`:

```rust
                None,
                Some(10), // max_iterations:brain 综合放宽到 10 轮(ReadOnly 默认 5 会在文件侦察上耗尽)
                crate::multi_agent::TaskComplexity::Simple,
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
git commit -m "feat(dual-brain): ask_brain 放宽 brain 会话至 10 轮并约束先综合后核实(机制3)"
```

---

### Task 3: 结构性兜底——超轮时返回部分结论提示而非"暂不可用"

**Files:**
- Modify: `crates/runtime/src/conversation.rs`(ask_brain 的 Err 分支)

- [ ] **Step 1: 改写 Err 分支**

现状:brain 会话超轮返回 `Err("subagent exceeded max_iterations ...")`,被降级为"brain 会话暂不可用 + 检索片段"——误导(超轮≠不可用),且掩盖了 brain 已产出的部分结论。将 Err 分支改为:

```rust
            Ok(result) => Ok(result),
            Err(e) => {
                // 机制 3 结构性兜底:brain 会话超轮或失败时,若已产出部分结论
                // (handoff 落盘于 .claw/subagents/brain-*.md),提示执行脑可去读取;
                // 同时附检索片段保证不阻塞。超轮≠不可用,文案须区分。
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

- [ ] **Step 2: 编译 + 既有测试回归**

Run: `cargo test -p runtime ask_brain 2>&1 | Select-Object -Last 12`
Expected: 全部 PASS(现有 degrades 测试断言"brain 会话不可用"的是**无 workspace_root** 分支,不经此 Err 路径,不受影响)。

- [ ] **Step 3: Commit**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "feat(dual-brain): ask_brain 超轮时提示部分结论落盘位置(机制3结构性兜底)"
```

---

### Task 4: clippy + 收尾(批次 1 验收)

**Files:**
- 无新增;全量验证

- [ ] **Step 1: clippy runtime**

Run: `cargo clippy -p runtime --all-targets -- -D warnings 2>&1 | Select-Object -First 15`
Expected: 无 error 无 warning(新增代码若触发 clippy,修复后重跑)。

- [ ] **Step 2: ask_brain 相关单测全量**

Run: `cargo test -p runtime ask_brain 2>&1 | Select-Object -Last 6`
Expected: PASS。

- [ ] **Step 3: 提交版本号更新(项目约定)**

提交前按项目约定更新 `rust/Cargo.toml` workspace version 为当天日期,随后 `cargo check` 刷新 Cargo.lock 并同一 commit 提交(按 project_memory 约定执行)。

- [ ] **Step 4: 最终 Commit(若 Step 3 有版本变更)**

```bash
git add rust/Cargo.toml rust/Cargo.lock
git commit -m "chore: bump version"
```

---

## Self-Review

**1. Spec coverage:** 机制 3(§4.5)三件套——Task 1(max_iterations 参数)+ Task 2(传 Some(10) + 文本预算约束)+ Task 3(超轮部分结论提示)✅。机制 4 与批次 2-4 **有意排除**(裁剪决策记录在案,spec §11 已同步标注暂缓)。

**2. Placeholder scan:** 无 TBD/TODO。Task 2 Step 1 的 `m.text_content()` 标注了 API 不确定时的备选(以实际 Session API 为准),非占位。

**3. Type consistency:** `run_subagent_turn_with_model` 新参 `max_iterations: Option<usize>` 插入位置(model 之后)在 Task 1/2 一致;`ctx.max_iterations: Option<usize>` 与 SubagentContext 字段一致。

**遗留(全部暂缓,恢复条件见文档头):** digest 写端门控、DigestRouter 读端、守护进程、EvidenceTracker、injection 通道、自治升级。
