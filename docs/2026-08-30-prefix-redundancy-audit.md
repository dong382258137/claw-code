# 前缀冗余审计(2026-08-30)

> 目标:识别当前请求前缀(DeepSeek 上下文缓存匹配区)中可移除的冗余字段。
> 背景:官方仪表盘今日缓存命中率 97.9%(输入命中 119,970,304 / 总输入 122,505,729)。
> 命中率已接近历史峰值 99%,剩余提升靠"前缀瘦身 + 前缀字节稳定性"。
> 状态:已逐条核实(2026-08-30);C 类已实施(2026-08-30,待实机验证)。

### 实机验证记录(2026-08-30 部署后)
- 部署:`cargo build --release -p rusty-claude-cli`(1m 07s)→ 覆盖全局 claw.exe
  (2026.8.30 / SHA cbdee580),`claw --version` 确认生效
- 会话 1(session-1788107389303):首轮 creation=28234(全新缓存,符合预期)
- 会话 2(session-1788107434936,21 轮真实调查任务):
  - **首轮 read=20992**(跨会话前缀复用 —— 第二个会话首轮即命中 20K 前缀)
  - 稳定轮次命中率 88-93%,尾 5 次 **91%**(vs 优化前同类会话 81%)
  - 中途 1 次波动(52%,creation=18K):固定记忆重建/压缩冷启轮,符合设计预期
- 全局(含优化前历史数据):95.61%(命中 12.5 亿 / 总 13.1 亿)
- 结论:前缀瘦身 + 字节稳定性优化生效 —— 跨会话首轮复用、长会话尾段命中率
  从 81% 提升至 91%。样本量小,需持续观察(新代码跑满 3-5 天后再看整体趋势)。

### 实测 TTL 机制(2026-08-31 追加,含悖论修正)
- **背景**:DeepSeek 官方未公布缓存 TTL(第三方口径不一:5 分钟刷新制 vs 几小时
  闲置清除制)。原 `FIXED_MEMORY_TTL_SECS = 300` 注释"对齐 DeepSeek 缓存 TTL"
  是未经证实的假设。
- **悖论修正**:初版尝试用"上一轮命中状态"(`!cache_hit`)驱动 LLM 重写,被指出
  逻辑悖论 —— **上一轮未命中恰恰说明它刚全量重建了缓存,本轮保持字节稳定即
  可命中;此时触发重写 = 主动打碎刚建立的缓存(双重浪费)**;上一轮命中则缓存
  仍热,重写同样打断。故"命中状态"不能作为触发判据。
- **最终方案(保守冷启间隔)**:
  - `conversation.rs` LLM 重写触发:距上次注入 > `FIXED_MEMORY_COLD_THRESHOLD_MS`
    (30 分钟,缓存几乎必然已冷)才触发 —— 成本摊进"反正要全量重建"的冷启轮
  - 不再依赖上一轮命中状态;`FIXED_MEMORY_PRECEDING_WINDOW_MS`(270s)降级为
    预留常量;300s TTL 仍作为规则路径 next_injection 的无实测兜底
  - TUI 长会话(间隔 < 30 分钟)几乎不重写 → 前缀超稳定;CLI 场景间隔远大于
    30 分钟 → 重写照常摊进冷启轮
  - 测试:触发测试改为 `fixed_memory_llm_triggered_after_cold_interval`
    (injected_at_ms 拨到 30 分钟前)
- **实机验证**:部署后新会话首轮命中率 92.6%(read=25,984 / creation=2,091)

---

## 0. 前缀结构全景

DeepSeek 上下文缓存按请求从头开始的连续字节序列匹配,因此缓存区 =
**system prompt(全部)+ messages 前部**。任何字节变化都使该点之后全部缓存失效。

```
【System prompt】prompt.rs build() L435-538,共 20 个 section
  static 区(boundary 前,17 段):
    intro / Output Style / System / Doing tasks
    Framework Switching / Transaction Safety / Memory Verification
    Context Recovery / Decision Experience / Cross-Session Recall
    Multi-Agent Orchestration / Session Bus / Agent Subagent Types
    Worker Lifecycle / Tool Usage Guidance / Persistent Memory / Repository Map
    Environment context / Runtime config / Claude instructions
  dynamic 区(boundary 后,同样参与前缀匹配):
    Project context(git 快照) / Available Skills / MCP Servers / Plan Mode Constraints

【Messages 前部(insert 0)】
  fixed_memory 快照   ← conversation.rs L3176 / L3208
  NOTEBOOK 稳定段      ← conversation.rs L3250
  历史消息...

【Messages 末尾(缓存边界外)】
  冻结槽位块(实时段)  ← conversation.rs L3256
```

**关键事实**:`fixed_memory` 与 NOTEBOOK 稳定段插在 messages 最前面,是"前缀中的
前缀" —— 字节一变,其后所有历史消息的缓存全部失效。**字节稳定性 > 体积**。

---

## A 类:直接重复(同义内容多处出现,可合并去重)

### A1. 日期出现 3 次 → **修正:2 处,可删 1 处**
- Environment `Date:` — prompt.rs L608(`environment_section`,static 区)
- Project context `Today's date is` — prompt.rs L750(dynamic 区)
- DynamicValueExtractor 提取 — cache_alignment.rs L48-141
- **核实**:DynamicValueExtractor 的 `classify_dynamic_value` 识别 `2026-08-30` 日期模式,
  将 Environment 的 `Date: 2026-08-30` 替换为 `<datetime_1>` 占位符,原值追加到
  dynamic 区 `# Cache-aligned dynamic values` 映射段(build_split L574-582)。
  实际呈现:占位符(static)+ 映射段 + Project context 原文,共 **2 处原文**。
  Project context 的 `Today's date is` 与映射段重复,可删。

### A2. Working directory 出现 2 次 → **成立**
- Environment `Working directory:` — prompt.rs L607(static)
- Project context `Working directory:` — prompt.rs L751(dynamic)
- **核实**:两处均为字面代码确认。删 Project context 处即可。

### A3. "记忆冲突时信文件" 3 处 → **成立**
- Memory Verification 第 3 条 — prompt.rs L1218
- Context Recovery 末行 — prompt.rs L1238
- fixed_memory 注脚"与当前对话不符时以最新对话为准" — fixed_memory.rs L203
- **核实**:3 处字面确认。前 2 处合并为 1,注脚保留(它在 messages 前缀,语义略不同)。

### A4. 历史检索引导 3 段重叠 → **成立(部分重叠)**
- Context Recovery — prompt.rs L1221(压缩后 recall_full + session_search 决策边界)
- Decision Experience — prompt.rs L1251(search_past_decisions / log_decision)
- Cross-Session Recall — prompt.rs L1272(跨会话 session_search 场景)
- **核实**:逐段比对,功能域不同(压缩找回 / 决策日志 / 跨会话回忆),但"何时检索
  历史"的引导语义高度重复。合并为 1 段可省 ~1K 字符,需保留各工具特定语义。

### A5. Framework Switching ↔ Transaction Safety 强耦合 → **成立**
- Transaction Safety — prompt.rs L1174-1188 两处引用
  ("see Framework Switching above"、"execution arm of the Framework Switching trigger protocol")
- **核实**:字面确认,强耦合。合并为 1 段可省 ~0.4K 字符。

### A6. "规划分水岭" ↔ Plan Mode Constraints 复述 → **成立**
- Tool Usage Guidance "规划机制分水岭" — prompt.rs L1603-1615
- Plan Mode Constraints — prompt.rs L1730-1738(整段复述分水岭)
- **核实**:Plan Mode Constraints 默认启用(`plan_mode().unwrap_or(true)`,prompt.rs L533),
  位于 dynamic 区末尾,每轮占 token,内容与分水岭段重复,可删。

### A7. 教训双通道 → **修正(部分成立)**
- fixed_memory "历史教训"块(lessons.jsonl)— fixed_memory.rs L150-156
- 冻结槽位块槽位 11 "Harness Lessons"(harness db)— conversation.rs L5521-5527
- **核实**:两通道**来源不同** —— harness db 记录 turn 级失败,lessons.jsonl 记录
  工具级瑕疵(压缩后提取)。并非完全重复。真正问题是 lessons.jsonl 已被压缩摘要
  污染(见 C2),应先净化,再评估是否合并。

### A8. Execution Style 槽位 ↔ Output Style → **修正**
- 冻结槽位块槽位 12 固定 "Be concise..." — conversation.rs L5531-5535
- **核实**:生产代码**无 `with_output_style` 调用**(仅 prompt.rs 测试 L2164/L2387),
  用户 .claw.json 亦未设置 → `# Output Style` section 不注入,与 Output Style 无重复。
  修正结论:该槽位是**每轮在尾部冻结槽位块重发的恒定内容**(~90 字符),与
  # Doing tasks 的简洁要求部分重叠。真正优化方向:恒定内容移入前缀 static 区,
  减少尾部每轮未命中 token(而非删除)。

---

## B 类:低频低价值但体积大(前缀瘦身)

### B1. git diff 快照 ≤8K 字符 → **成立**
- MAX_GIT_DIFF_CHARS=8000 — prompt.rs L701
- render_project_context 注入 git status + recent commits + git diff — L747-787
- **核实**:`render_project_context` 在 dynamic 区;build() 一次性构建,session 内
  字节稳定,跨 session 变化(新会话首轮不命中属合理)。模型极少直接消费 diff,
  体积大价值低。建议:移除 git diff,仅保留 `git status --short` + commits。

### B2. Repository Map → **成立(≤1024 tokens)**
- render_repomap_section — prompt.rs L1659-1664,注入 static 区
- **核实**:app.rs L2926-2949 —— 非宽泛目录时启用,`RepoMap::with_max_tokens(1024)`;
  用户 chanlun 工作区(D:\chanlunV2\chanlun_py)非宽泛目录 → **启用**,体量 ≤1024 tokens。
  建议:截断为深度 ≤2 目录树(收益有限,非优先项)。

### B3. Runtime config 全量 JSON → **部分成立**
- render_config_section — prompt.rs L990-1021,`config.as_json()` 全量渲染(脱敏后)
- **核实**:代码层面确为全量渲染;但用户当前 .claw.json 仅 61 字节(仅 permissions),
  实际体积小,收益有限。建议:代码层面改为 feature 开关摘要(防御未来配置膨胀)。

### B4. Multi-Agent Orchestration 示例块 → **成立**
- 两个示例 JSON 块 — prompt.rs L1349-1362
- Model Selection Guide — L1378-1436(表格 + 决策树 + 示例)
- **核实**:共 ~1.4K 字符;示例代码块(如 spawn_parallel_subagents 调用)模型几乎
  不会照抄。建议:删示例 JSON,保留决策树与表格。

---

## C 类:前缀稳定性隐患(真正的命中率杀手,优先级最高)

### C1. fixed_memory 含高熵易变字段,却插在 messages[0] → **成立**
- 请求构造 insert(0) — conversation.rs L3176 / L3208
- **核实**:实测 `D:\chanlunV2\chanlun_py\.claw\fixed_memory.json` content 为 LLM 简报,
  含"当前目标 / 已完成项 / 历史教训 / **下一步**"—— "当前目标/下一步"随任务推进
  变化;300s 前瞻触发(conversation.rs L3131-3134)重建后字节全变 → 其后全部历史
  消息缓存失效。建议:易变"当前目标/下一步"移到尾部冻结槽位块,前缀只留低频
  稳定内容(已完成项/教训)。

### C2. lessons.jsonl 被压缩摘要污染 → **成立(机制+数据双重确认)**
- 写入链路 — conversation.rs L7163-7174 → lessons.rs `parse_lessons_from_summary` L40-59
  → `append_lessons` L63
- **核实(机制)**:`apply_lessons_from_compaction` 用 `parse_lessons_from_summary` 从压缩
  摘要提取 `[lessons]` 段,`extract_section(summary, "[lessons]", "")` 的 end_marker 为
  空(取到文本结尾),**无质量过滤**—— LLM 在 [lessons] 段填入的非教训内容原样入库。
- **核实(数据)**:实测 `lessons.jsonl` 30+ 条中绝大多数是摘要残余:
  "Newly compacted context:" / "Scope: 66 earlier messages compacted" /
  "Tools mentioned:" / "Recent user requests:" / "Pending work:" / 对话转述,
  真正教训仅 1 条。
- 影响:fixed_memory "历史教训"块每次重建字节抖动 → 前缀变化。
- 建议:parse_lessons_from_summary 加质量过滤(拒绝摘要头/字段名/对话转述),
  或由 LLM 明确输出纯教训。

### C3. task_state.closed_tasks 混入 [lessons] 标签与教训文本 → **成立**
- 写入链路 — conversation.rs L7137-7144(`apply_task_state_from_compaction` 直接 push)
- **核实(机制)**:压缩后 `apply_task_state_from_compaction` 从摘要 `[closed_tasks]` 段
  提取并**无过滤** push 进 state.closed_tasks;LLM 字段混淆时(把 lessons 也列在
  closed_tasks)即混入。
- **核实(数据)**:实测 `task_state.json` closed_tasks 含 "[lessons]"、
  "使用 `git stash` 做基线测试对比时..."、"完成提示词质量初步审查(PASS)"等非任务条目。
- 影响:fixed_memory completed 列表冗余 + 字节不稳定。
- 建议:apply_task_state_from_compaction 加过滤(跳过 `[lessons]` 标签行/非任务文本)。

---

## 实施记录 —— C 类优化(2026-08-30)

### C2:lessons.jsonl 质量过滤(已实施)
- `lessons.rs` `parse_lessons_from_summary` 逐行经 `is_lesson_like` 过滤:
  - 黑名单 `SUMMARY_RESIDUE_PREFIXES`(17 个摘要结构头前缀:Newly compacted context /
    Scope / Tools mentioned / Recent user requests / Pending work / ...)
  - 拒绝 markdown 标题/分隔(`#` / `|` / ```)、纯符号超短行(有效字符 < 4)
- 新增测试 `parse_filters_summary_residue` / `parse_filters_markdown_and_short_lines`
- **存量清理**:`D:\chanlunV2\chanlun_py\.claw\lessons.jsonl` 30 条 → 3 条
  (保留 2 条 B 类操作教训 + 1 条猜测循环教训;备份 `lessons.jsonl.bak-20260830`)

### C3:task_state closed_tasks 过滤(已实施)
- `task_state.rs` 新增 `is_task_completion_like`:拒绝 `[xxx]` 标签行(如 `[lessons]`)与
  教训句式(含 "应使用" / "不要 " / "需先")
- `parse_task_state_from_summary` 的 closed_tasks 解析接入过滤
- 新增测试 `parse_summary_filters_label_and_lesson_lines_from_closed_tasks`
- **存量**:`task_state.json` 已混入的 3 条残留会在下次压缩解析时被跳过(历史数据
  由 fixed_memory 重建自然淘汰,未主动改写用户文件)

### C1:fixed_memory 高熵字段移尾部(已实施)
- `fixed_memory.rs` 新增 `split_stable_volatile(content) -> (String, Option<String>)`:
  - volatile = `当前目标` / `下一步` 块(任务推进高频变化)
  - stable = 其余(已完成项 / 历史教训 / 注脚,低频稳定)
  - 兼容 LLM 简报(块式)与规则简报(`- 当前目标:` bullet)两种格式
- `conversation.rs` 请求构造两条注入路径(LLM 触发 L3176 / 规则 L3208)改为注入
  **stable 段**到 messages[0];stable 为空时退化注入完整内容
- `conversation.rs` `render_runtime_hints` 新增**槽位 4:Current Task**(从
  `self.fixed_memory` 拆分 volatile 注入尾部冻结槽位块)
- 效果:重建 fixed_memory 只改尾部消息,前缀(已完成项等低频内容)字节稳定
- 新增测试 `split_stable_volatile_partitions_llm_brief` / `_rule_brief` / `_no_volatile_returns_none`;
  更新 4 个受影响的 e2e 断言(head 不再含"当前目标",goal 移至尾部槽位)

### B1:砍掉 git diff 快照(已实施)
- `prompt.rs`:
  - `ProjectContext.git_diff` 字段删除(定义 + 3 处构造)
  - `discover_with_git` 不再调用 `read_git_diff`(源头控制,启动不再跑 `git diff`)
  - `render_project_context` 不再注入 `Git diff snapshot:` 块
  - 删除 `read_git_diff` / `truncate_diff_to_budget` / `read_git_output` /
    `MAX_GIT_DIFF_CHARS` / `GIT_DIFF_TRUNCATION_MARKER`
- `conversation.rs`:4 处 `git_diff` 字段引用清理(子代理上下文 + 2 处测试构造)
- 测试:原 `discover_with_git_includes_diff_snapshot_for_tracked_changes` 改写为
  `discover_with_git_skips_diff_snapshot`(验证 status 保留、diff 不注入);
  删除 2 个 `truncate_diff_to_budget` 单元测试;use 列表清理
- 效果:每会话首轮前缀最多减 8K 字符(实际为工作区 diff 大小);跨会话变化源
  移除,新会话首轮 creation tokens 减小
- `git status --short` + recent commits 保留(模型定位改动仍可用)

### A1/A2:删 Project context 重复日期与工作目录(已实施)
- `prompt.rs` `render_project_context`:删除 `Today's date is` 与 `Working directory`
  两行 —— 两者已由 `# Environment context`(static 区,DynamicValueExtractor
  占位符化 + `# Cache-aligned dynamic values` 映射段)提供
- 空段防御:`render_project_context` 无内容(无 instruction 文件且无 git 信息)时
  返回空串,`build()` 调用点跳过注入,避免孤立 `# Project context` 标题
- 子代理环境层(conversation.rs L599-600)不受影响:子代理 system prompt 独立,
  无 Environment section,仍需自身注入 Working directory / Date
- 效果:Project context 段减 2 行;日期/cwd 信息不丢失(映射段仍完整)

### A3:删"记忆冲突信文件"重复行(已实施)
- `prompt.rs` Context Recovery 末行 `When a memory conflicts with actual file contents, trust the files.` 删除
- Memory Verification 第 3 条(同语义)保留;fixed_memory 注脚保留

### A4:历史检索 3 段三合一(已实施)
- `get_context_recovery_section` 重写为 `## History & Decisions (历史检索与决策日志)`:
  压缩后找回(recall_full)+ 决策边界 + 决策日志(DecisionLog,search_past_decisions/log_decision)
  + 跨会话回忆(session_search/NOTEBOOK plan)
- 删除 `get_decision_log_section` / `get_cross_session_recall_section` 函数与 build() 调用
- 保留全部工具特定语义,精简重复"何时检索"引导,省 ~0.6K 字符

### A5:Transaction Safety 并入 Framework Switching(已实施)
- Transaction Safety 内容并入 Framework Switching 段末尾为 `### Rollback (Transaction Safety 事务保护)` 子节
- 删除 `get_transaction_safety_section` 函数与 build() 调用

### A6:删除 Plan Mode Constraints(已实施)
- build() 中 plan_mode 检查注入块删除;`render_plan_mode_constraint_section` 函数删除
- 运行时 plan_mode 功能(Plan/Execute/Review 循环、EnterPlanMode 等)不受影响
- 删除 3 个 plan_mode constraint 注入测试

### A7:教训双通道(已由 C2 覆盖)
- 两通道来源不同(harness db turn 级失败 vs lessons.jsonl 工具级瑕疵),保留双通道;
  lessons.jsonl 污染已由 C2 净化,不再合并

### A8:Execution Style 移入前缀 static(已实施)
- conversation.rs 冻结槽位块槽位 12(Execution Style 常量)删除 —— 恒定内容不再
  每轮在尾部重发(按未命中计费)
- prompt.rs 新增 `get_execution_style_section()`,注入 # Doing tasks 之后(static 区)
- 注:Execution Style 原为冻结槽位块的"保底槽位"(无条件注入);移除后,当某轮
  所有条件槽位均空时尾部不再有冻结槽位块 —— 合理行为(无内容不渲染),
  有内容时仍渲染(真实场景 NOTEBOOK/Current Task/plan 等通常非空)

### B2:RepoMap 单文件 symbol 渲染上限(已实施)
- repomap.rs 新增 `MAX_RENDER_SYMBOLS_PER_FILE = 8`:render_files 每文件最多输出
  8 个 lsp_symbols/definitions,防止符号密集型文件独占 1024 token 预算
- 注:审计原建议"深度 ≤2 目录树"不适用 —— 当前 RepoMap 是"文件列表+符号"
  格式(非目录树),且已有 max_tokens=1024 预算截断;改为单文件符号上限更贴合

### B3:Runtime config 全量 JSON → 功能开关摘要(已实施)
- render_config_section 不再渲染 `config.as_json()` 全量 JSON,只渲染摘要:
  `model` / `permission_mode` / `planMode` / `poorMode`(存在才列出)
- 防御配置膨胀 + 减少低价值 token(用户当前 .claw.json 仅 61 字节,收益小,
  但代码层面已杜绝未来配置膨胀进 prompt)
- 脱敏函数 redact_sensitive_json 等暂无生产调用,保留为防御代码(#[allow(dead_code)]
  + 注释说明,测试仍覆盖)
- 更新 2 处测试断言(permissionMode → permission_mode)

### B4:Multi-Agent 段删示例 JSON 块(已实施)
- 删除 `**Example** — a dependency pipeline`(dag_define 示例,~14 行)与
  `**Example** — User: 分析三个模块...`(spawn_parallel_subagents 示例,~16 行)
- 保留:Tool Selection Guide / Model Selection 表格、两个决策树、Key principle、
  DAG Workflow Pattern 步骤、Pre-defined DAGs 说明
- 省 ~0.6K 字符

### 验证
- `cargo test -p runtime`:1921 passed / 1 failed(activity_monitor 偶发失败,与本次
  改动无关,单独重跑通过)/ 2 ignored
- `cargo clippy -p runtime --all-targets`:新改动文件零 warning
- 待办:实机使用观察命中率(fixed_memory 重建轮的前缀稳定性),对照
  `_analyze_cache_hit.py`

---

## 建议实施顺序

1. **C 类优先**(影响命中率本身):
   - C2 净化 lessons 源(parse_lessons_from_summary 加质量过滤)
   - C3 剥离 closed_tasks 伪标签与教训文本
   - C1 高熵"当前目标/下一步"移入尾部冻结槽位块
2. **B 类次之**(省钱,不提高命中率):B1 砍 git diff 收益最大
3. **A 类最后**(文字级清理):A1/A2 删 Project context 重复日期/cwd 最简单直接

---

## 核实记录(2026-08-30)

| 项 | 结论 | 修正说明 |
|----|------|----------|
| A1 | 修正 | 3 处 → 实际 2 处原文(DynamicValueExtractor 已把 Environment 日期占位符化),可删 Project context 处 |
| A2 | 成立 | cwd 字面 2 处,删 Project context 处 |
| A3 | 成立 | 3 处字面确认 |
| A4 | 成立 | 部分重叠,合并可省 ~1K,需保留工具特定语义 |
| A5 | 成立 | 强耦合,合并可省 ~0.4K |
| A6 | 成立 | Plan Mode Constraints 默认启用且复述分水岭,可删 |
| A7 | 修正 | 两通道来源不同(非完全重复);重点是先净化 lessons.jsonl(C2) |
| A8 | 修正 | 生产代码无 with_output_style → 与 Output Style 无重复;恒定内容每轮尾重发 → 移前缀或删 |
| B1 | 成立 | git diff ≤8K,session 内稳定、跨会话变,价值低体积大 |
| B2 | 成立 | 非宽泛目录启用,≤1024 tokens,用户工作区启用 |
| B3 | 部分成立 | 全量渲染但用户配置仅 61 字节,收益小 |
| B4 | 成立 | 示例块可删,省 ~1.4K |
| C1 | 成立 | LLM 简报含高熵"当前目标/下一步",重建字节变,插 messages[0] |
| C2 | 成立 | 机制(无质量过滤)+ 数据(30+ 条摘要残余)双重确认 |
| C3 | 成立 | 机制(无过滤 push)+ 数据([lessons] 标签混入)双重确认 |
