# 2026-09-03 双大脑认知分离架构设计 v3

> 状态:定稿 v3(架构师最高决策权裁决) · 任务 1 · 质量第一,成本次之
> 前序: 草案 v2 → 双验证(P1 网络 6 要点 / P2 本地 12 项)→ 三问题裁决(本版核心变更)

## 1. 背景与北极星

自我进化系统(harness_evolution × decision_log × memory × NOTEBOOK)架构级重构。北极星:管理脑实时在线辅助执行脑——投喂它所需知识/文件/代码,执行脑**聚焦任务决策**;管理脑同时承担架构守护。

### 架构层转向
- **错误路线**: 知识注入执行脑上下文 + 补丁闸门——注入是单脑架构固有属性,膨胀/注意力稀释/验证难题/成本线性增长是注入的**必然结果**。
- **架构解**: 双脑 = **空间分离 + 问答通道**。知识资产归管理脑;执行脑上下文恒定小;无广播注入,只有问答/消息。

## 2. 目标架构(三角色)

```
┌─ L0 执行脑(主会话)────────────────────────────┐
│ 上下文 = 系统提示(最小,静态收敛) + 任务 + 工具结果 + 收到的答案   │
│ 聚焦:任务决策。不持有知识资产。                      │
│ 知识入口: ① ask_brain(LLM 综合,按需) ② 确定性检索     │
│ 工具(零 LLM 查字典) ③ 失败单条辅导(自动消息)           │
└───────────────┬───────────────────────────┘
                │ ask_brain / 确定性检索 / 失败信号
┌───────────────▼───────────────────────────┐
│ L1 管理脑(按需实例化的 brain 会话,ReadOnly)       │
│ 独有知识资产:.claw/ 下全部                    │
│ 治理(消化/提炼/验证) = 管理脑私有事务            │
│ 只提议,不直改(无写工具);输出带来源+置信度            │
└───────────────┬───────────────────────────┘
                │ 行动结果(编译/测试/用户验收)
┌───────────────▼───────────────────────────┐
│ L2 验证层:行动结果隐式验证一切(不可妥协的仲裁者)      │
└────────────────────────────────────────────┘
```

## 3. ★三问题裁决(第一性原理 + 代码证据)

### Q1: ask_brain 是否"唯一知识入口"? → **否,修正 v2 的概念错误**
- 第一性原理: 执行脑认知需求分两类,成本差两个数量级:
  - **查字典**(确定性检索): "这错以前修过没"——答案在库,LLM 综合是浪费 → 0 LLM
  - **求综合**(LLM 推理): "我这设计哪错了"——需跨库上下文+推理 → 1 次 brain 会话
- 若 ask_brain 唯一入口,查字典也烧 brain 会话 → 每次查字典付一次 LLM,违背成本原则。
- **北极星是"执行脑不持有知识",不是"不能调确定性检索"**。确定性检索不携带知识,只是提问手势。
- 代码证据: search_past_decisions(7162)零 LLM 直查 decision_log;session_search(4965)直查 FTS5——都是"查字典",保留。
- **裁决**: 保留确定性检索工具(零 LLM);ask_brain 是 LLM 综合入口(需推理时才用)。瘦身的是**常驻注入**,不是提问能力。

### Q2: 执行脑瘦身范围 → **停全量注入;保留失败触发的最小辅导**
- 第一性原理: 全量注入的意图 = "失败后让执行脑知道教训"。只停不补则丢意图;广播改消息则意图保留形式合理。
- 代码/数据证据: harness_edits 表**当前 0 行**(实测 .claw/decision_log.db)——全量注入面实际为空,**此刻瘦身零迁移成本,唯一无痛窗口**;auto_capture_failure(1023)记账管道已存在。
- **裁决**: 停槽位 11 render_for_injection 全量注入(conversation.rs:6037-6044);失败辅导走"下轮单条注入"(组件②)——与 Q1 的查字典同构,确定性检索。

### Q3: 设计 v2 的其他修正 → **三处,全部有代码证据**
1. **brain 能力层错误**: v2 写"Execute cap + ReadOnly 白名单"——但白名单由 capability **决定**(multi_agent/mod.rs:101-119: Execute 自带 edit_file/write_file/bash),不可叠加。brain 持写工具 = 管理脑可改代码 = 违背"只提议不直改"。
   → **修正: brain 用 ReadOnly cap**(只读 5 工具,迭代上限 5,max_iterations 证据 multi_agent/mod.rs:132),综合质量靠注入检索片段 + Architectural SOP。
2. **digest 无消费点**: v2 只写落盘没人读 = 白写。
   → **修正: digest 消费点 = 新会话 gate_task 前注入**(knowledge_freshness 钩子 conversation.rs:1655)——首 turn 感知上一会话遗留,不进 system_prompt 静态段(缓存保护)。
3. **ask_brain 执行路径未定**: v2 含糊。
   → **修正: 同步调用 run_subagent_turn_with_model(5664,非异步 spawn 免轮询),带 cost_limit(dispatch 已支持,5071 区域)。**

## 4. 现状分析(代码事实,全部 ✅ 已核实)

| 组件 | 位置 | 现状 | 标记 |
|---|---|---|---|
| 工具路由链 | conversation.rs:3991-4110 | if-else 拦截内建工具 | ✅ |
| 工具定义 | conversation.rs:162-537 | `XXX_TOOL_SPEC` raw JSON schema 常量 | ✅ |
| 同步问答原语 | conversation.rs:5664 `run_subagent_turn_with_model` | 同步一次子代理 LLM 调用 | ✅ |
| 统一执行入口 | conversation.rs `execute_subagent_llm` | 路径 A/B 共用 guard/轮次/prompt | ✅ |
| brain 提示构建 | conversation.rs:590 | capability 分层;Architectural SOP | ✅ |
| brain 上下文 | conversation.rs:6062 | repo_map/env 提取;白名单过滤 | ✅ |
| 能力白名单 | multi_agent/mod.rs:101-119 | Analyze=∅/ReadOnly=5 只读/Execute=+写 | ✅ |
| 失败→下轮注入 | conversation.rs:1587 `pending_remediation` | 上轮失败→下轮追加→消费即清 | ✅ |
| 会话结束钩子 | conversation.rs:4901/4908/4915 | HookEvent::Stop / mark_plan_stale / SessionEnd | ✅ |
| harness 全量注入 | harness_evolution/mod.rs:759 | Active edits 无条件全量,无闸门 | ✅ |
| 失败自动记账 | conversation.rs:1023 | auto_capture_failure:签名+simhash+Pending | ✅ |
| 会话开始钩子 | conversation.rs:1655 gate_task | knowledge_freshness 事前注入点 | ✅ |
| 成本机制 | dispatch cost_limit(5071);poor_mode;usage.rs | 现成 | ✅ |
| **数据: harness_edits** | .claw/decision_log.db | **0 行**(瘦身窗口期) | ✅ |
| **数据: decisions** | .claw/decision_log.db | **12 行**(Confirmed 10/Pending 2),有 knowledge_source 列 | ✅ |
| 知识资产 | .claw/ 下全部 | 管理脑可读 | ✅ |

## 5. 组件设计(定稿)

### 5.1 ask_brain 工具(LLM 综合入口,按需)
- **执行脑视角**: `{"query": "...", "context": "...", "k": 5}` → 带来源聚合答案。
- **实现**:
  1. 确定性检索 top-k: decision_log(同签名)+ harness Active + NOTEBOOK + session FTS5,simhash/Jaccard 去重;
  2. 拼 brain 任务文本(片段含来源路径+置信度);
  3. **同步调用 run_subagent_turn_with_model(5664)** spawn brain 会话(**ReadOnly cap** + Architectural SOP;可再读文件核实);
  4. 返回答案:每条结论带来源;cost_limit 护栏;usage 计入成本。
- **失败降级**: 检索空 → "无相关知识";brain 失败 → 返回原始检索片段(不带综合),不阻塞。

### 5.2 失败单条辅导(实时在线,零 LLM)
- 工具失败 → auto_capture_failure 记账(已有)→ **下轮自动检索同签名历史决策** → 有高质量决策(verify_count≥1 且有外部验证证据)→ **单条**注入 dynamic_sections(消费即清);无 → 不注入(宁缺毋滥)。

### 5.3 会话 digest(架构守护)
- HookEvent::Stop(4901)后确定性聚合: 失败 signature 计数/决策记录/工具用量+成本行/遗留 TODO;
- 落盘 `.claw/management_brain/digest-<session_id>.md`;
- **消费点(v3 修正)**: 新会话 gate_task 前注入首 turn 感知(conversation.rs:1655 钩子),不进静态段。

### 5.4 执行脑瘦身
- **停槽位 11**(conversation.rs:6037-6044)render_for_injection 全量注入(harness_edits 现 0 行,零迁移);
- 知识资产不进 system_prompt 静态段;
- 保留确定性检索工具 + ask_brain + 失败辅导(瘦注入,不瘦能力)。

## 6. 质量治理与信任边界

- **执行脑信任**: 管理脑输出 = 带来源**建议**(不可信输入,CaMeL 原则);采信由行动结果(编译/测试/验收)隐式裁决。
- **管理脑只读**: brain 会话 ReadOnly cap(无写工具),判断错误 = 单次可归因建议,改不了代码。
- **知识库写入权**: 仅确定性管道(auto_capture_failure/digest)+ 验证层;LLM 自评永不当"已验证"。
- **可证伪验收**: 对照组 vs 实验组跑 N 真实任务,测成功率/重试数/成本;不显著优 → 自动投喂默认关,保留 /consult 人工面。

## 7. 成本模型

| 成本项 | 做法 |
|---|---|
| 执行脑每轮 token | 瘦身后面恒定小(静态收敛,省全量注入) |
| 查字典 | 确定性检索工具,0 LLM |
| 求综合(ask_brain) | 显式触发才实例化;带 cost_limit;零常驻 |
| 失败辅导 | 0 LLM(确定性检索单条) |
| digest | 0 LLM(确定性聚合) |
| 缓存 | brain 结果不污染执行脑前缀;注入走 dynamic_sections 末尾追加 |

## 8. 实现可行性评审(9 项,基于 v3 修正)

1. **签名兼容**: execute_ask_brain(&mut self, input:&str)(同步外壳)→ async 内部调 run_subagent_turn_with_model;与 dispatch_subagent 同构;✅
2. **参数来源**: query/context/k 来自 JSON;workspace_root 从 self;✅
3. **数据传递链**: query → execute_ask_brain → 检索器(decision/harness/notebook/session)→ 拼文本 → run_subagent_turn_with_model → 答案;无跨层丢失;✅
4. **判定优先级**: 同签名优先最新/最高 verify_count;空→降级;brain 失败→原始片段;✅
5. **retry/重入**: ask_brain 无状态;复用 execute_subagent_llm max_attempts/cost_limit;✅
6. **冲突处理**: brain ReadOnly 无写面,与执行脑改文件无冲突;✅
7. **与现有系统重叠**: 停全量注入(5.4);auto_capture_failure 保留(上游);search_past_decisions 保留(查字典);✅
8. **失败路径**: 检索库损坏→空;brain 失败→片段降级;digest 写失败→静默;均不阻断;✅
9. **成本估算**: ask_brain ~180 行(检索器+execute+路由+spec);失败辅导 ~70 行(复用 pending_remediation);digest ~130 行(聚合+落盘+首 turn 注入);瘦身 ~15 行(注释消费点);合计 ~400 行 + 测试 ~350 行;✅

## 9. 分阶段实施(每批独立可测)

- **批次 1 核心闭环**: ask_brain 工具(检索器 + brain 会话 ReadOnly + 路由 + spec)+ 执行脑瘦身(停全量注入)—— 单测检索/降级/成本护栏 + 端到端 ask 一次。
- **批次 2 实时辅助**: 失败单条辅导(复用 pending_remediation 模式)。
- **批次 3 架构守护**: 会话 digest 聚合器 + 落盘 + 首 turn 消费注入。
- **收尾**: 全量回归(clippy 0 / workspace check)+ log_decision + 收尾。

## 10. 诚实的不确定性

- 检索 top-k 召回精准率未经真实数据量化——批次 1 用现有 12 条 decisions 测。
- "停注入→执行脑自主性↑"因果未端到端验证——可证伪验收(§6)裁决。
- brain 综合质量依赖子代理 LLM 能力——Architectural SOP + ReadOnly 护栏设最低质量线。
