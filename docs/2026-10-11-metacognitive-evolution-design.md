# 热升级元认知自动进化闭环设计

> 日期:2026-10-11
> 状态:设计(待实施 P0–P4)
> 关联:`docs/2026-09-03-dual-brain-architecture.md`(管理脑)、`docs/2026-10-09-metrics-anchoring-design.md`(度量口径)
> 目标:**让 claw 在实时任务中自行感知"框架需改进",把感知成果归档到源码根,唤醒一个专职进化子智能体,由其改码→验证→热升级→部署全局。**

---

## 1. 问题陈述

现状:热升级的**机械链路**已闭环,但**"为什么要升级"这一前端完全缺失**。

| 环节 | 现状 | 证据 |
|---|---|---|
| 触发 | `HotUpgrade` 工具置位 → turn 收尾后 `run_upgrade()` | `tools/src/lib.rs:2459`、`rusty-claude-cli/src/app.rs:2782` |
| 执行 | 持久化 plan → 写 `.claw/upgrade-request.json` → spawn helper → 退出 | `app.rs:2800-2830` |
| 助手 | 等退出 → mv `exe→.old` → `cargo build -p rusty-claude-cli` → 回拷原路径 → 启动 | `upgrade_helper.rs:167-268` |
| 恢复 | 新进程 resume + 恢复 plan + 自动续跑 | `maybe_resume_from_upgrade`(提交 `d1d34f7c`) |

**缺口**:谁来发现"框架缺能力"?目前只有**人**。即 `HotUpgrade` 工具存在,但**没有触发它的上游信号**——agent 除非被明确指示,不会自省"框架本身需要改"。

## 2. 目标闭环(五段)

```
[感知] 实时任务中,agent 或框架发现"框架级缺陷/优化/缺功能"
   ↓
[存档] 归档到 <源码根>/.claw/evolution/inbox/*.md
   ↓
[唤醒] inbox 新增文件 → 唤醒专职进化子智能体
   ↓
[进化] 子智能体(源码根)读提案 → 改码 → 测试门 → 提交
   ↓
[部署] 热升级(build+回拷) → 全局 ~/.cargo/bin/claw.exe 生效
```

---

## 3. 组件设计

### 3.1 P0 前置:源码根注册表(整个闭环的地基)

**硬缺口**:claw 在任意目录(如 `D:\chanlunV2`)执行任务时,**无法得知源码根在 `D:\claw-code-src`**。`upgrade_helper` 现从 `user_cwd` 推源码根,在任意目录下**必然失败**(`upgrade_helper.rs:184-186` 注释自认)。

**设计**:`~/.claw/source-root` 单行文本文件,记录源码根绝对路径。

解析优先级:
1. 环境变量 `CLAW_SOURCE_ROOT`(最高,便于测试/多源码树)
2. `~/.claw/source-root`(注册表)
3. 从当前 exe 上溯找 `Cargo.toml`(源码构建场景回退)
4. 从 `user_cwd` 上溯(旧行为,保留)

写入时机:`claw init` 或首次在源码树内运行热升级时自动登记。

**产出**:
- `runtime/src/source_root.rs`:`resolve_source_root() -> Option<PathBuf>`、`register_source_root(&Path)`
- `upgrade_helper` 改为优先用注册表(修复"任意目录热升级")

### 3.2 P1 感知:两级门控(廉价本地信号 + 元认知总结)

**核心约束**:每次收敛都跑 LLM 元认知 = 每 turn 多一次往返,成本不可接受。**必须分级**。

**A. 廉价本地信号(零 LLM)** — 复用 `c871b109` 的"本地预筛短路"模式:

| 信号 | 来源 | 语义 |
|---|---|---|
| 用户打回 | `convergence::classify_punchback` | 上一轮答复被否定 |
| 循环检测 | `LoopDetector` 命中 | agent 反复同动作 |
| 工具连败 | tool_result `is_error` 连续 ≥N | 环境/框架持续失败 |
| 超软阈值 | `TURN_TIME_SOFT_SECS`(提交 `7c27a6e6`)、迭代软阈值 | 效率异常 |
| 任务失败 | `record_turn_failed`(`conversation.rs:9655`) | 有明确失败信号 |

**B. LLM 元认知总结** — 仅在 A 命中时触发:

- 复用 `management_brain`:`extract_session_summary:49` / `render_digest:81` / `write_digest:135`
- 收尾钩子已有接入点:`record_turn_completed:9605`、`record_turn_failed:9655`
- 输出**结构化提案**(不是自由文本):见 §4 模板

**C. 进行中通道(零额外 LLM)** — agent 中途自觉"框架缺能力"时,直接落盘:

- 新工具 `report_framework_gap`(轻量,类似 `notebook_update`)
- **对比**:元认知总结是"事后复盘",本通道是"即时观察"——两者互补,后者几乎零成本

### 3.3 P2 唤醒:inbox 扫描(无守护进程 v1)

**机制缺口**:全仓**无文件监视器**(`notify`/`inotify` 均无;`session_bus::watch` 是会话总线,非文件)。

**v1 方案 α(推荐,零新进程)**:
- 归档写 `<源码根>/.claw/evolution/inbox/*.md`
- **扫描时机**:① claw 在源码根启动时;② 显式 `claw evolve` 子命令;③ **任意目录任务收尾且已登记源码根时**(后台 fire-and-forget)
- 处理完的提案 `mv` 到 `inbox/processed/`

**v2 方案 β(终态,可选)**:常驻 `claw-evolve-daemon` 用 `notify` 监视 inbox → 新文件即 spawn 子智能体。**标为 P4,非必需**。

### 3.4 P3 进化子智能体

**可复用**:`Agent` 工具、`TaskRegistry::spawn_subagent_for_task`(`task_registry.rs:171`)。

**职责**(在源码根 `cwd` 下):
1. 读 inbox 提案 → 定位相关源码
2. 改码(复用现有工具链)
3. **测试门**:`cargo test -p <crate>` + `cargo fmt --check` + `cargo clippy`
4. 提交(不 push)
5. 触发 `HotUpgrade` → 助手 build+回拷 → 全局生效

**安全边界**(硬性):
- **测试必过才装**:否则一个坏编辑会 brick 全局 claw(循环依赖:用 claw 改 claw)
- **黑名单**:不得自动改 权限/安全/测试判据/升级助手自身
- **人工闸门**:v1 默认"改码+测试通过→停在待装",需 `claw evolve --install` 才部署

### 3.5 P4 自动部署 + 守护进程(可选终态)

- 文件锁(防多实例并发 `cargo build`/exe 覆盖)
- 常驻守护进程(β)
- 自动 `--install`(默认仍建议保留人工闸门)

---

## 4. 提案文档格式(存档契约)

`<源码根>/.claw/evolution/inbox/<ts>-<slug>.md`:

```markdown
---
id: <session_id>-<counter>
created_at_ms: 1234567890
source_cwd: D:\chanlunV2            # 触发目录(任意)
source_session: session-xxx
trigger: punchback | loop_detected | tool_fail_streak | soft_threshold | task_failed | agent_reported
evidence:
  - "<可执行判据,如 cargo test -p runtime 失败输出>"
severity: low | medium | high
category: bug | perf | feature | dx
---

## 观察
<一段话:框架在哪里表现不佳>

## 证据
<文件:行号 / 命令输出 / 指标数值>

## 建议
<可选:初步修改方向>

## 验收判据
<改完怎么算成功(可执行)>
```

**关键**:`evidence` 必须是**可执行/可复现判据**,不是"感觉"——与 `docs/2026-10-09-metrics-anchoring-design.md` 的诚实性红线一致。

---

## 5. 落地阶段与风险

| 阶段 | 内容 | 新增 | 风险 |
|---|---|---|---|
| **P0** | 源码根注册表 + 修复任意目录热升级推导 | `source_root.rs` | 无 |
| **P1** | 廉价信号门控 + 元认知总结 + `report_framework_gap` 工具 | 门控模块 + 1 工具 | 低 |
| **P2** | inbox 扫描 + `claw evolve` 子命令 | CLI 子命令 | 中 |
| **P3** | 进化子智能体(改码→测试门→提交) | 子智能体提示 + 编排 | **高** |
| **P4** | 文件锁 + 守护进程 + 自动部署 | daemon | 高 |

### 五个硬约束(必须正视)

1. **循环依赖**:进化智能体用 claw 改 claw。改坏 → build 失败 → 只能靠 `exe.old` 手工回滚。⇒ **测试门是硬前提**。
2. **并发竞争**:多目录多实例同时归档 → 并发 build/exe 覆盖。⇒ **文件锁**(P4)。
3. **归因困难**:区分不了"框架缺陷"vs"任务本就难"(实测 `runtime-6f4c0e4b` 即例)。⇒ 提案要求**可执行判据**;元认知提示须含"先排除任务难度"。
4. **成本**:每次收敛都元认知 = 每 turn 一次往返。⇒ **必须门控**(A 级信号)。
5. **自改安全**:不得自动改 权限/安全/测试判据。⇒ 黑名单 + 人工闸门。

---

## 6. 已确认的决策(用户授权按建议执行)

1. **触发时机**:**收敛后 + 失败后**两者都做,共用同一门控。
2. **部署闸门**:v1 **先进化+测试通过、等人确认再 `claw evolve --install`**;不默认自动装全局。

---

## 7. 验收方式(本闭环自身如何度量)

- P0:任意目录下热升级能正确找到源码根(测试)
- P1:门控不触发时**零额外 LLM 往返**;触发时产出**格式合法**提案(测试)
- P2:`claw evolve` 能列出/处理 inbox(测试)
- P3:进化子智能体对**已知缺陷任务**能完成 改码→测试过→提交(trace 证据)
- P4:文件锁防并发(测试)

## 8. 与既有 `harness_evolution` 的边界(审查 R1 发现,必须划清)

**审查 R1 发现**:`runtime/src/harness_evolution/`(mod 53KB + archive 34KB + types) **已实现自进化 harness**(design-gaps #2,遵循 `docs/2026-07-24-p3-self-evolving-harness-design.md`),含弱点挖掘(`mine_weaknesses:109`)、LLM 提议 trait(`HarnessProposer`)、两重门控(`validate_candidate:426`)、归档(`HarnessArchive` → `decision_log.db::harness_edits`)、CLI `claw harness evolve --dry-run`(`commands_handler.rs:1001`)。

**它进化的不是源码**——`grep -c "write_file\|fs::write" harness_evolution/mod.rs` = **0**;它产出的是**注入 `SystemPromptSplit::dynamic_sections` 的规则文本**(`render_for_injection:760`)。

| 维度 | `harness_evolution`(已有) | 本设计(新增) |
|---|---|---|
| 进化对象 | prompt 注入规则 | **Rust 源码** |
| 改源码 | 否(0 处) | 是 |
| 生效方式 | 下次注入即生效 | 需 build + 热升级 + 回拷 |
| 存储 | `decision_log.db::harness_edits` | `.claw/evolution/inbox/*.md` |
| 风险 | 低(仅 prompt) | **高(可 brick 全局)** |

**结论:互补,非重复。但必须复用其成熟模式**(而非另造一套):
- 弱点挖掘 → 复用 `mine_weaknesses` 的 pathology 聚类思路
- 双重门控 → 本设计 §3.4 的"测试门"是其对应物(源码级用 cargo test 替代 z-test)
- 归档表 → 本设计用文件(md)而非 sqlite 表,因提案需**人可读可审**(人工闸门前提)
- **不重复**:不新建第二个 `claw evolve` 顶层命令;源码级入口命名 `claw evolve-source`(或 `claw evolve --source`),与既有 `claw harness evolve` 区分。

---

## 9. 三次客观审查结论(2026-10-11)

### 审查 R1 — 轮子重造检查(发现 1 项重大)
- ✅ **发现 `harness_evolution` 已存在** → 已在 §8 划清边界并确立复用策略。
- ✅ 确认 `source_root.rs` / `report_framework_gap` / inbox 机制**均不存在**(非重造)。
- ✅ `claw harness evolve` 存在但范围不同(read-only、prompt 规则)。

### 审查 R2 — 事实正确性
- ✅ 设计引用行号**逐条核对准确**:`app.rs:2782 run_upgrade`、`tools/lib.rs:2459/2465`、`management_brain.rs:49/81/135/152`、`conversation.rs:9605/9655`、`hooks.rs:44`。
- ✅ 既有自进化文档(`2026-07-24-p3-self-evolving-harness-design.md`)经 grep **未覆盖源码级/热升级** → 本设计填补真空白。
- ✅ `~/.claw/settings.json` 无 `source_root` 字段 → 无命名冲突。

### 审查 R3 — 实施前提与风险
- ⚠️ **无文件锁基建**(`flock`/`FileLock` grep 为空)→ P4 需新增;但 P0–P3 不依赖。
- ⚠️ **无 `notify`/`fs2`/`fd-lock` 依赖** → v1(P2 方案 α)**故意不引入**文件监视(靠启动/显式扫描),避免新依赖;`notify` 仅 P4 守护进程才需。
- ✅ `Agent` 工具 schema 含 `cwd` 字段(`tools/lib.rs:1650`)→ P3 进化子智能体可指定 `cwd=<源码根>`,无需新机制。
- ✅ 启动路径有明确接入点:`lib.rs:533 maybe_resume_from_upgrade`(P2 扫描可挂此处)。

### 审查后的 3 处设计修订
1. **§8 新增**:明确 `harness_evolution` 边界与复用清单;顶层命令改名避免冲突。
2. **§3.3 强化**:P2 v1 **明确不引入 `notify`**;守护进程(需 notify)推后到 P4 且标为可选。
3. **§5 风险表更新**:P4 拆出"文件锁"为独立前置(当前无基建)。

---

## 参考来源

- 热升级链路:`rusty-claude-cli/src/app.rs:2782`、`upgrade_helper.rs:167-268`
- 管理脑:`runtime/src/management_brain.rs:49/81/135/152`;接入 `conversation.rs:3823/6221/6252/6258`
- 收尾钩子:`conversation.rs:9605/9655`;`hooks.rs:44`(HookEvent)
- **既有自进化**(划界对象):`runtime/src/harness_evolution/mod.rs:109/426/674/760`;`docs/2026-07-24-p3-self-evolving-harness-design.md`
- 本地预筛先例:提交 `c871b109`(分诊本地预筛短路)
- 诚实性红线:`docs/2026-10-09-metrics-anchoring-design.md` §5
