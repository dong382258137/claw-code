# 未实现设计内容 × 完善后收益清单

> 生成日期：2026-08-09
> 来源：对 `D:\claw-code-src\docs` 全部设计方案与源码实现状态的对比核查
> 用途：作为后续实施排期与优先级决策的依据
> 核查结论要点：docs 中大部分设计方案已实现并默认开启；以下为**未实现 / 部分实现 / 已偏离**项的收益清单。
>
> **实施进度（2026-08-09）**：#3 知识新鲜度（S4 负向排除 + 中文关键词，commit `ca22cc4c`）、#4 提示词引导（WebSearch 优先 / ToolSearch / TaskUpdate，commit `222f1b2f`）、#1 hooks（matcher + timeout，commit `4ffc45bc`；PostCustomToolCall 事件面，commit `17a9598e`）已完成；#3 剩余路径 A 传递链（`knowledge_source` 移除全局 last-gated，改 LLM 显式传参精确传递）、#4 的 capability/`--resume`（经核查代码不存在）未实现。各条目以 ✅ 标注。

---

## 核查结论速览

- 已全部实现（9 份）：subagent 前缀缓存对齐、subagent TRAE 对齐（Epic 1 部分）、TUI 工具结果折叠、TUI MD 表格换行、TUI 输出智能化、tool-edit-reliability PRD、harness-engineering-optimization（+phase2）、multi-agent-hardening、loop-prevention、design-headroom-absorption。
- 部分实现（4 份）：ide-hooks-dag-implementation-plan、modules/dag-orchestration-detail、modules/hooks-system-detail、modules/ide-integration-detail。
- 未实现 / 已偏离（2 份）：2026-07-24-p3-self-evolving-harness（完全未开工）、local-openai-compatible-providers（文档已过时，代码改为 DEEPSEEK_* 专有）。
- 默认开启状态：绝大多数功能默认开启；仅 TUI 模式（`--tui` + full-tui feature）、poor_mode、PolicyEngine（`--enable-policy-engine` 且 P1-2/P1-3 未接入）需显式开启。

---

## 🔴 P0 级 — 收益最高（直接影响任务成功率与产品完整性）

| # | 未实现内容 | 完善后的收益 |
|---|---|---|
| 1 | **hooks 系统四件套缺失**：matcher 正则过滤、每 hook 独立 timeout、异步 HookRunner、配置热重载（`runtime/src/hooks.rs` 仅同步 Command/Script/Http/Mcp） | ① 命令/脚本 hook 卡死**不再阻塞整个对话循环**（当前同步执行是单点故障源）；② hook 可按工具名/事件精准触发，避免误拦截；③ 改 hooks.toml 无需重启会话，运维成本大降；④ 防止恶意/失控 hook 无限期挂起 |
| 2 | **self-evolving harness 完全未开工**：`harness_evolution` 模块、`task_success` 字段、`claw harness` CLI（`docs/2026-07-24-p3-self-evolving-harness-design.md`） | ① 会话失败教训自动沉淀 → **跨会话自我进化**，同类错误不再重复犯；② TraceAnalyzer 聚类 + 自动修复建议形成闭环；③ 这是"从工具到自进化 Agent"的分水岭能力 |
| 3 | ✅ **知识新鲜度门控三处偏差（已全部完成）**：~~S4 负向排除~~、~~关键词表无中文词~~、~~路径 A 传递链降级为全局 last-gated~~（`runtime/src/knowledge_freshness.rs`）。**已完成**：S4 负向排除 + 中文关键词（commit `ca22cc4c`）；**已完成**：路径 A 传递链 —— `knowledge_source` 移除全局 last-gated（并发串任务根因），改由 LLM 基于自身任务上下文显式传参，主 agent 默认 `parametric` | ① 消除**内部仓库任务误触发联网调研**的 token 浪费（当前"在这个仓库里改 X"可能被判 Novel）；② 中文任务（本项目主力场景）新鲜度评估准确率显著提升；③ `knowledge_source` 统计不再串任务，决策闭环可信 |
| 4 | ✅ **AI 提示词 4 处能力缺失（部分实现）**：~~WebSearch 优先~~、~~知识新鲜度~~、capability 参数、会话恢复协议。**已完成**：WebSearch 优先 + 知识新鲜度引导写入 `runtime/src/prompt.rs` Tool Usage Guidance（commit `222f1b2f`）；**不实现**：capability 参数（`AgentInput` 无该字段）与 `--resume`（claw-shell 无该参数）——代码核查确认不存在，避免引导幻影功能 | ① 子智能体能力分级（analyze/read-only/execute）**从"代码里有、AI 不会用"变为可引导调用**；② Novel 任务自动触发调研，减少过时知识自信错答；③ 明确"先搜索后回答"约束；④ AI 知道 `--resume` 会话恢复语义。**纯提示词改动、零架构成本，却是收益杠杆最高的项** |

---

## 🟡 P1 级 — 收益中高（补齐已承诺的能力，消除"半成品"）

| # | 未实现内容 | 完善后的收益 |
|---|---|---|
| 5 | **TRAE 对齐 Epic 1 剩余**：`SubagentContext.tool_summaries` 生产路径留空（`runtime/src/conversation.rs:3916` 注释"暂留空"） | 子智能体 system prompt 的 `## Available Tools` 层当前**真空**，注满后：① 子 agent 清楚自己有哪些工具可用，不再试错调用被拒；② L2 工具层进静态前缀，复用缓存断点，不损命中率 |
| 6 | **ACP/IDE 应用层 5 项**：`fs/read_text_file`、`fs/write_text_file`、`session/request_permission`、LaneEvent→SessionNotification 桥接、VS Code 扩展（`claw-shell/src/agent.rs` 这些方法为 stub） | ① **IDE 闭环**：claw 作为 ACP server 可读编辑器缓冲区、请求用户权限，VS Code 扩展让 AI 直接编辑当前打开文件；② 桥接后 IDE 端实时感知 lane 进度；③ 这是文档承诺的"IDE 原生体验"，补齐后 claw 不再是纯终端工具 |
| 7 | **hooks 的 Stop/PostCustomToolCall 集成点缺失**（hooks.rs 有方法但 conversation.rs 未接入） | 会话停止、自定义工具调用完成时 hook 静默失效。接入后：监控/审计类 hook 覆盖完整事件面，合规场景才可用 |
| 8 | **local-openai-providers 文档漂移**：文档写 `OPENAI_BASE_URL`，代码已改为 `DEEPSEEK_*` 专有（`api/src/providers/mod.rs:162-164` 无条件 DeepSeek 路由） | 文档-代码对齐后：① 用户不再按过时文档配置导致失败；② 若需支持 Ollama/OpenRouter 等，可恢复通用 OPENAI_BASE_URL 分支，**解锁本地模型调试场景**（离线/低成本开发） |

---

## 🟢 P2 级 — 收益中（锦上添花，非阻塞）

| # | 未实现内容 | 完善后的收益 |
|---|---|---|
| 9 | **DAG 声明式增强**：YAML 加载器、条件边、CheckpointStore 断点续跑、PlanArtifact→DAG 转换、mermaid 渲染（`dag/` 已收敛进 types.rs，这些子文件未展开） | ① YAML 声明式 DAG + 条件边 → **复杂工作流可配置化**，无需改代码；② checkpoint 断点续跑 → 长 DAG 失败不重头再来；③ 可视化渲染降低编排调试成本。注：dag_run 核心调度已可用（`tools/src/lib.rs:3652`），此项是增强非必需 |
| 10 | **PolicyEngine 接入未完成**（仅 flag 开启且打日志，P1-2/P1-3 未接线） | lane 完成策略评估真正生效，避免子智能体"声称完成但质量不达标"被直接采信 |

---

## 🟣 工具使用缺口 — 能力存在但 AI 从未调用

> 与上述设计缺口同源：都是「已承诺能力未释放价值」。区别：设计缺口是"代码里没有"；**本清单是"代码里已实现，但全部会话工作流从未调用"**。来源：`.claw/sessions` 全量 tool_use 审计——56 个内置 mvp 工具仅实际调用 24 种，约 33 种从未被 AI 主动使用。
>
> 注意：以上计数基于工具广告列表（56 个）；实际可用还包括 runtime 内部工具（session_search、dispatch_subagent、verify_decision、query_project_graph、get_symbol_info、refactor_algorithm_topo 等），其中 `verify_decision`（学习环闭合）与 `refactor_algorithm_topo`（符号重命名建议）也均为 0 次。

### 🔴 P0 级 — 收益最高（零成本、立即可用）

| # | 未调用工具 | 未释放的收益 |
|---|---|---|
| 11 | ✅ **WebSearch / WebFetch**（0 次 → **已引导**，commit `222f1b2f`） | 排查外部错误、查 API 文档/版本变更时「先搜索后回答」，替代"盲猜 + 反复编译"的试错循环；当前工作流全程未联网检索 |
| 12 | **Agent**（0 次，**部分**：既有 `Agent Subagent Types` 段已覆盖 subagent_type 选择，未新增"替代 dispatch_subagent"引导） | 子智能体委派 + 持久化 handoff 元数据；当前仅用内部 `dispatch_subagent`/`spawn_parallel_subagents`（各 1 次），跨 turn 保留子任务产物的能力闲置 |
| 13 | ✅ **ToolSearch / SkillSearch**（0 次 → **已引导**，commit `222f1b2f`） | 工具发现门卫：56+ 工具 AI 未必全知道，主动搜索可避免"不知道有工具而用 bash/grep 绕道" |

### 🟡 P1 级 — 补齐闭环（任务进度与编排）

| # | 未调用工具 | 未释放的收益 |
|---|---|---|
| 14 | ✅ **TaskUpdate**（0 次 → **已引导**，commit `222f1b2f`） | TaskCreate 3 次 / TaskOutput 5 次但任务状态从不更新，进度闭环不完整 |
| 15 | **CronCreate / CronDelete / CronList**（0 次） | 定时任务（周期回归、每日报告），调度基础设施已就绪 |
| 16 | **dag_run / dag_status**（0 次） | DAG 编排生产路径已实现（`tools/src/lib.rs:3652`），批处理/流水线场景完全闲置 |
| 17 | **Worker 系列**（Create/Get/Observe/SendPrompt/Restart/Terminate 等 9 个，0 次） | 长驻编码 worker + trust gate，适合大型多轮改造 |

### 🟢 P2 级 — 按场景启用

| # | 未调用工具 | 未释放的收益 |
|---|---|---|
| 18 | **LSP**（0 次） | 语言服务器诊断/符号分析，需配置 server |
| 19 | **MCP / ListMcpResources / ReadMcpResource / McpAuth**（0 次） | 外部 MCP 工具链，需配置 server（im-bridge 已接通链路） |
| 20 | **StructuredOutput / EnterPlanMode / RunTaskPacket / REPL / SendUserMessage / NotebookEdit / glob_search / TeamCreate/Delete / RemoteTrigger / ImBridgeSetup/Service**（0 次） | 结构化输出、计划模式、任务包、REPL、主动推送、笔记本、glob、团队协作、远程触发等特定场景增强 |

---

## 建议优先级排序（按投入产出比）

```
立即做（低投入高收益）：
  ① ✅ 提示词补全 4 能力（部分：WebSearch/知识新鲜度引导已提交 `222f1b2f`；capability/`--resume` 因代码不存在跳过）
  ② ✅ 知识新鲜度负向排除 + 中文词（已提交 `ca22cc4c`）
  ③ ✅ hooks matcher + timeout（已提交 `4ffc45bc`）→ 收敛当前同步 hook 的单点故障风险
  ③' ✅ hooks PostCustomToolCall 事件面（已提交 `17a9598e`）
  ③'' ✅ 知识新鲜度路径 A 传递链（移除全局 last-gated → LLM 显式传参，`knowledge_source` 不再串任务）

短期做（中等投入）：
  ④ ACP fs/permission + 桥接  →  解锁 IDE 闭环
  ⑤ self-evolving harness MVP  →  立项级，建议拆成 harness 记录 + CLI 两个里程碑
  ⑥ tool_summaries 注满  →  子 agent 能力可见性
  ⑥' hooks 剩余：异步 HookRunner（P1 异步化，文档建议 P0 保留同步）+ 配置热重载

按需做（有明确场景再投入）：
  ⑦ DAG YAML/checkpoint/渲染
  ⑧ PolicyEngine 接线
  ⑨ OPENAI_BASE_URL 恢复通用 provider
```

### 工具使用缺口排序（与上表同批决策）

```
立即做（零成本、与提示词补全同批）：
  ⑩ ✅ WebSearch/WebFetch 写入「先搜索后回答」提示词约束（已提交 `222f1b2f`）
  ⑪ Agent 工具替代内部 dispatch_subagent 的引导（待办，`Agent Subagent Types` 段已部分覆盖）
  ⑫ ✅ ToolSearch 作为工具发现门卫（已提交 `222f1b2f`）

短期做（补齐闭环）：
  ⑬ ✅ TaskUpdate 补任务状态更新（已提交 `222f1b2f`）
  ⑭ Cron 定时任务用于周期回归/每日报告  →  调度设施已就绪，直接启用

按需做（有明确场景再投入）：
  ⑮ dag_run / Worker* 用于批处理与长驻编码 worker
  ⑯ LSP / MCP* 需先配置对应 server
```
