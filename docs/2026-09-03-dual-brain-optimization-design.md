# 2026-09-03 双大脑架构优化设计(brain-daemon 独立进程版)

> 状态:设计定稿(brainstorming 获准) · 前置审查:2026-09-03-dual-brain-architecture.md(v3)
> 本设计修正 v3 的角色定位:管理脑从"ReadOnly 按需顾问"升格为"常驻、有写权、主导 claw 自身架构升级的守护进程"。

## 1. 背景与设计动机

原双大脑 v3 经审查发现四项缺陷(详见评估):
1. **验证环断裂**:决策库 verify_count 全为 0,`top_verified_solution` 要求 `Confirmed + verify_count≥1` 恒不满足 → 失败单条辅导"设计有、永不触发"。
2. **管理脑无主动性**:管理脑只在执行脑 ask_brain 时被动应答;北极星"实时在线辅助执行脑"未兑现。
3. **brain 会话截断**:ReadOnly max_iterations=5,实测 brain 会话 5 轮耗尽在文件侦察上,返回降级片段而非综合答案。
4. **digest 相关性不足**:按文件修改时间取最新,而非按价值/相关性。

**角色再定义(用户裁决)**:
- **执行脑** = 干活的大脑:接用户任务、改代码、跑测试、交付结果。焦点:任务执行,干完活让验证层(编译/测试/验收)裁决。
- **管理脑** = 系统守护者:持有+治理本地知识与资产(决策库/会话史/digest/规则);**主导 claw 自身架构升级(亲自实施,验证层裁决)**;执行脑干活时**主动旁听辅助**(失败/重复才介入)。

## 2. 目标架构

```
┌─ 执行脑(claw CLI 主会话)───────────────────────────┐
│ 接用户任务 · 改代码 · 跑测试 · 交付                    │
│ 焦点:任务执行(干完活 → 验证层裁决)                     │
│ 知识入口:ask_brain / 确定性检索 / 失败单条辅导 / digest  │
└───────────▲──────────────────────────────┬────────┘
            │ ③主动投喂(失败/重复)          │ ②按需 ask_brain
┌───────────┴──────────────────────────────▼────────┐
│ 管理脑(brain-daemon,常驻独立进程)                    │
│  A 持有+治理知识资产(决策库/会话史/digest/规则)          │
│  B 主导 claw 架构升级:亲自实施 → 验证层裁决            │
│  C 主动旁听:静默监视,失败/重复才投喂                   │
└────────────────────────────────────────────────────┘
            ▲ 验证层(编译/测试/用户验收)仲裁一切
```

- 执行脑不再承担自主进化责任,进化责任移交管理脑。
- 管理脑是**常驻、有写权、自我进化的守护者**,而非"会话后消化器"或"ReadOnly 顾问"。
- 两脑通过 `.claw/` 共享媒介通信;验证层是共同仲裁。

## 3. 关键决策记录(brainstorming 用户裁决)

| 决策点 | 裁决 |
|---|---|
| 优化范围 | 四项全做(验证环/主动旁听/brain截断/digest相关性) |
| 角色定位 | 执行脑=干活;管理脑=知识资产+claw架构升级+辅助执行脑 |
| 架构升级对象 | claw 自身代码 + 领域知识资产 |
| 升级实施方式 | **亲自实施**(管理脑持写工具),验证层裁决 — 推翻 v3 的 ReadOnly |
| 组织载体 | **独立第二进程**(brain-daemon,与 claw-im-bridge 同构) |
| LLM 使用 | 允许轻 LLM 消化(守护进程可用小/预算模型) |
| 辅助模式 | 主动旁听辅助(非拉取式) |
| 旁听介入策略 | **失败/重复才介入**(正常推进不动嘴) |
| 验证信号 | 同签名演进:连续 N 次未见失败即视为被绕开(N=3) |
| digest 选取 | 按任务相关度检索(非时间取最新) |
| 实施分批 | 4 批独立可测 |

## 4. 组件设计

### 4.1 brain-daemon(新独立二进制 / crate)

```
brain-daemon(常驻进程)
├─ SessionWatcher   尾随 session_bus 事件/文件,监听执行脑工具流
├─ FailureMatcher   失败签名 vs decision_log,触发投喂判定
├─ EvidenceTracker  同签名演进:跨会话计数"连续未见失败 N 次"→ 更新 verify_count
├─ DigestRouter     按任务相关度检索 digest(替代时间取最新)
├─ DigestGovernor   写端重要性门控(去噪)
├─ BrainAgent       LLM 综合/提炼(轻模型):知识提炼、架构升级执行
└─ UpgradeRunner    架构升级亲自实施(Execute 能力,验证层裁决)
```

每个单元职责单一、接口清晰、可独立测试。与执行进程仅通过 `.claw/` 共享媒介通信,无 in-process 调用。

### 4.2 机制 1:验证环闭合(同签名演进,零 LLM 确定性)

- 状态死锁:v3 要求 `Confirmed + verify_count≥1`,无人调用 verify_decision → 永不触发。
- 演进判定(守护进程 EvidenceTracker,零 LLM):
  1. 扫描会话工具结果,提取失败签名 `(tool, sig)`;
  2. 查决策库同签名 Pending 条目(simhash 已有);
  3. **后续连续 N 个会话(默认 N=3)该签名未再出现为失败** → 视为"被绕开/修复" → Pending 递增验证计数;
  4. 规则:须有外部信号(该工具调用成功 / 用户显式确认)佐证,单次未出现不构成证据。
- 效果:verify_count 自然增长 → `top_verified_solution` 条件满足 → 失败单条辅导复活。

### 4.3 机制 2:主动旁听辅助(brain-daemon)

- SessionWatcher 实时同步执行脑工具结果流(经 session_bus 事件或文件尾随);
- FailureMatcher 静默比对:失败模式 vs 知识库(decision_log / 历史 FTS5);
- **触发条件**:
  - 高匹配已验证方案(阈值命中)→ 投喂单条(复用 `pending_failure_advice` 机制,dynamic_sections 注入);
  - 重复失败(同签名 ≥2 次)→ 预警;
  - 正常推进 → 静默,零开销;
- 投喂走 dynamic_sections 尾部注入,不污染 prefix 缓存。

### 4.4 机制 3:brain 会话截断修复(快修,执行进程内)

- `ask_brain` 调用 `run_subagent_turn_with_model` 时传 `ctx.max_iterations` 覆盖(8~10,现为 ReadOnly 默认 5);
- task 文本约束:"先综合后核实,最多核实 2 个文件";
- 输出预算:强制返回"结论+依据"格式,避免中途截断成降级片段。

### 4.5 机制 4:digest 治理

- **写端(DigestGovernor)**:重要性门控,只记录"重复失败/高价值结论/成本异常",去噪;
- **读端(DigestRouter)**:新会话首 turn 以任务目标为 query 检索 digest(本地相似度),替代 `latest_digest_text` 按时间取最新。

## 5. 数据流

```
执行脑工具流 → (session_bus/文件) → SessionWatcher
  ├─→ FailureMatcher → 命中 → 投喂注入(单条 dynamic_sections)
  ├─→ EvidenceTracker → 连续未见失败 N 次 → decision_log.verify_count++
  └─→ DigestGovernor → 会话结束写门控 digest
新会话首 turn → DigestRouter(任务相关度检索)→ 注入 "Previous Session Digest"
```

## 6. 错误处理与降级

- 守护进程任何失败 → 静默降级,绝不影响执行脑主链路;
- 守护进程崩溃 → 无状态,重启重扫(幂等);
- decision_log 写冲突 → 复用既有 SQLite 事务;
- 轻 LLM 不可用 / poor_mode → 守护进程退化为纯确定性(仅 watcher/matcher/evidence,不做语义提炼)。

## 7. 测试策略

| 组件 | 测试 |
|---|---|
| EvidenceTracker | 合成失败序列,验证 N 次未见失败后计数递增;单次未出现不计 |
| DigestRouter | 多 digest,验证相关度优先于时间 |
| FailureMatcher | 命中/未命中/重复失败阈值 |
| DigestGovernor | 噪音会话不产出 digest;重复失败必产出 |
| brain 截断修复 | ask_brain 端到端返回综合答案(非降级片段) |
| 并发隔离 | 守护进程与执行脑并发写 decision_log 无冲突 |

## 8. 分批实施(每批独立可测)

- **批次 1(快修,执行进程内)**:brain 会话截断修复(迭代覆盖+输出预算)+ digest 写端门控。
- **批次 2(守护进程骨架)**:brain-daemon 二进制 + SessionWatcher + FailureMatcher + 投喂注入。
- **批次 3(验证环)**:EvidenceTracker 同签名演进 → verify_count 自然增长 → 失败辅导复活。
- **批次 4(治理与升级)**:DigestRouter 相关度检索 + DigestGovernor 完善 + BrainAgent/UpgradeRunner(轻 LLM 提炼 + claw 架构升级亲自实施)。

## 9. 诚实的不确定性

- 连续未见失败 N 次作为"被绕开"证据是启发式:可能漏判(修复靠改环境而非改调用)或误判(任务变化导致不再触发)。批次 3 用真实决策库回测验证。
- 主动旁听投喂频率未知:阈值需在批次 2 以真实会话校准,宁缺毋滥。
- 管理脑亲自实施 claw 架构升级的风险:与用户项目工作区隔离需明确(管理脑仅对 claw workspace 有写权)。批次 4 先行试点低风险升级验证流程。
