# Active Plan 瘦身与去重设计

> 日期：2026-11-27
> 状态：已实现（`a371693d` / `64e3285a`），待重建二进制生效
> 关联决策：dl-150、dl-151

## 1. 问题

`messages` 尾部每轮重新注入的块**无法命中 prompt cache**，按全价计费。
实测（本会话 `req-00126.json`）：

| 块 | tok/轮 | 性质 |
|---|---|---|
| 固定记忆简报 | 338 | 全价 |
| Project context（尾部） | 168 | 全价 |
| Runtime Context 合计 | 1,903 | 全价 |
| └ `## Active Plan` | **1,419** | 全价，**最大单点** |
| └ `## Notebook` | 219 | 全价 |
| └ `## Current Task` | 130 | 全价，**与 Active Plan 重复** |
| └ `## Step Status` | 28 | 全价 |
| **合计** | **2,409** | |

对照：system + tools 共 20,978 tok，但整体落在 cache 断点内（system@[11,14,17]、
tools@[46]/共47），按命中价 ≈ 2,098 折算单位。
**尾部 2,409 tok 比整个 system+tools 更贵。**

## 2. 两处缺陷

### 2.1 `Task:` 行重复渲染

`render_skeleton()` 输出 `Task: <task_summary>`，而尾部槽位 4「Current Task」
（`conversation.rs` 槽位 4）**逐字渲染同一份** `task_summary`。

现场证据（本会话注入的 Runtime Context）：

```
## Active Plan
Task: 修订计划（原 plan-1795649565638-5760 …）
...
## Current Task
修订计划（原 plan-1795649565638-5760 …）
```

同一段 434 字符文本每轮付两次全价。

**为什么不能删槽位 4**：`current_task_slot_uses_active_plan_not_workspace_snapshot`
测试守护它——该槽位优先取 `active_plan.task_summary` 而非工作区 `task_state.goal`，
防止**旧任务目标泄漏**进当前上下文（2026-08-31 实证的回归）。
故正确方向是**保留槽位、移除骨架侧重复**。

### 2.2 PRE-COMMITMENT 模板按步重复

原实现在 `for step in steps` 循环内为每个 high-risk 步骤输出 3 行协议正文：

```
   ⚠ PRE-COMMITMENT REQUIRED: This step is high-risk (destructive/irreversible/security-sensitive).
   Before implementing, generate 2+ candidate approaches and briefly state trade-offs (cost, risk, reversibility).
   Only commit after explicit comparison — never commit to the first feasible option just because it is feasible.
```

实测 329 字符 ≈ 91 tok/份。本会话计划有 4 个 high-risk 步 ⇒ 4 份 ≈ 366 tok，
其中 3 份是**纯重复**。

## 3. 方案

### 3.1 选定：聚焦式瘦身（方案 B）

| 方案 | 做法 | 节省 | 风险 |
|---|---|---|---|
| A 全量瘦身 | 删 Current Task + 每步只留一行 | ~1,080 tok | **高**：模型不知道当前步骤的 acceptance/verify，无法判断何时算完成 |
| **B 聚焦式瘦身（选定）** | 删骨架 `Task:` 行 + PRE-COMMITMENT 提到循环外 | ~486 tok | 低：当前步骤信息完整 |
| C 只删重复 | 仅删 Current Task | 130 tok | 零，但放弃 350+ tok |

A 被否决的原因：`acceptance` / `verify` 是模型判断「这一步做完没有」的唯一依据，
删掉会导致假报完成（违反铁律 I5）。B 保留全部判断依据，只消除逐字重复。

### 3.2 实现

`render_skeleton()`：

1. 移除 `out.push_str(&format!("Task: {}\n\n", self.task_summary))`；
   由尾部槽位 4 独家负责渲染任务摘要。
2. 循环内仅 `high_risk_seen = true` 标记；循环结束后统一输出一次协议正文：
   ```
   ⚠ PRE-COMMITMENT REQUIRED for the high-risk step(s) marked ⚠ above:
   ```
   high-risk **归属**仍由每步的 `⚠ high-risk` 标签表达，信息不丢失。

## 4. 影响面

| 影响 | 说明 |
|---|---|
| `render_skeleton` 输出 | 少 `Task:` 行、少 N-1 份协议正文 |
| `render_for_prompt` | **未改动**（仅测试调用，生产路径走 `render_skeleton`） |
| `Current Task` 槽位 | **未改动**，防泄漏语义完整保留 |
| `Step Status` 槽位 | 未改动（每轮变化的 `[plan status] 1=done 2=pending`） |
| `Project context` | **未改动**——见下节 |

### 4.1 为什么不动 Project context 双份

实测 `Project context` 在 system `block[17]`（983ch，带 cache_control）与
messages 尾部（463ch，全价）各存一份，看似重复。**但不能删尾部那份**：

`conversation.rs::refresh_project_context_section` 的注释明确写着
「id=61 修复：用实时 git 状态刷新注入的 `# Project context` 段（原地替换）」。
system 内那份是会话起始的**冻结快照**，尾部这份**每轮用实时 git 状态刷新**——
这正是 dl-76（快照陈旧导致 agent 花轮次证伪）的修复。

删除它会让 dl-76 的修复回退。若日后要优化，只能考虑「system 内那份删掉、
保留尾部实时份」，但那会让 983ch 从命中价变全价，**反而更贵**。
故本轮不动，记录为已知设计。

## 5. 收益

fixture 对比（`tests/fixtures/active_plan_{before,after}.json`）：

| 指标 | before | after | 降幅 |
|---|---|---|---|
| total_tail_tokens | 238.9 | 187.9 | **-21.3%** |
| active_plan_tokens | 137.7 | 86.7 | **-37.0%** |
| current_task_tokens | 10.7 | 10.7 | 0（保留防泄漏槽位） |
| project_context_tokens | 25.3 | 25.3 | 0（见 4.1） |

真实会话量级（本会话实测）：
- 重复的 `Task:` 行 434ch ≈ 212 tok/轮
- PRE-COMMITMENT 3 份重复 ≈ 274 tok/轮
- **合计 ≈486 tok/轮（全价区）**

价值对比：静态区 -322 tok @命中价 ≈ 32 折算单位；尾部 -486 tok @全价 ≈ 486 单位。
**尾部去重的价值是静态区精简的 15 倍。**

## 6. 回滚

单 commit 可回滚：

```
git revert a371693d   # 去重实现
git revert 64e3285a   # 回归测试
```

回滚后 `Task:` 行与逐步 PRE-COMMITMENT 恢复，无数据迁移、无配置项、无状态残留。

## 7. 验证

- `cargo test -p runtime --lib` → 2269 passed（2 个 bash 计时测试 flaky，单独跑通过，与本改动无关）
- `cargo clippy -p runtime --all-targets -- -D warnings` → clean
- `scripts/fmt.sh --check` → exit 0
- 新增 3 个回归测试锁定行为契约：
  - `render_skeleton_omits_task_line_to_avoid_duplicate_with_current_task_slot`
  - `render_skeleton_emits_precommitment_once_for_multiple_high_risk_steps`
  - `render_skeleton_omits_precommitment_when_no_high_risk_step`

## 8. 未做项与理由

| 项 | 结论 | 理由 |
|---|---|---|
| 工具 schema 延迟加载 | **不做** | tools[46] 带 cache_control，47 个工具整体按命中价计费，收益仅 ~934 命中价单位，却要承担「模型读到指令却调不到工具」风险 |
| 删除尾部 Project context | **不做** | 是 dl-76 / id=61 的修复，删了会回退 |
| 删除 Current Task 槽位 | **不做** | 防旧任务目标泄漏，有测试守护 |
| 静态区继续压缩 | **收益极低** | 命中价计费，-322 tok 仅值 ~32 单位 |

## 9. 后续可做（未在本轮）

`## Active Plan` 仍占 86.7 tok（fixture）/ ~1,000 tok（真实长计划），
主因是每步完整渲染 `acceptance` + `verify`。下一步可考虑：
**仅焦点步（Executing，否则首个 Pending）保留完整 detail，其余折叠为一行**，
预计再省 ~600 tok/轮全价。风险：需确保模型能从 `.claw/plans/*.json` 按需读回详情。
