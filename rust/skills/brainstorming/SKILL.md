---
name: brainstorming
description: "You MUST use this before any creative work - creating features, building components, adding functionality, or modifying behavior. Explores user intent, requirements and design before implementation."
---

# Brainstorming Ideas Into Designs

Help turn ideas into fully formed designs and specs through natural collaborative dialogue.

Start by understanding the current project context, then ask questions one at a time to refine the idea. Once you understand what you're building, present the design and get user approval.

<HARD-GATE>
Do NOT invoke any implementation skill, write any code, scaffold any project, or take any implementation action until you have presented a design and the user has approved it. This applies to EVERY project regardless of perceived simplicity.
</HARD-GATE>

## Anti-Pattern: "This Is Too Simple To Need A Design"

Every project goes through this process. A todo list, a single-function utility, a config change — all of them. "Simple" projects are where unexamined assumptions cause the most wasted work. The design can be short (a few sentences for truly simple projects), but you MUST present it and get approval.

## Process

Complete these steps in order:

1. **Explore project context** — check files, docs, recent commits
2. **Ask clarifying questions** — one at a time, understand purpose/constraints/success criteria. Prefer multiple choice when possible. Only one question per message — if a topic needs more exploration, break it into multiple questions.
3. **Propose 2-3 approaches** — with trade-offs and your recommendation. Lead with your recommended option and explain why.
4. **Present design** — in sections scaled to their complexity, get user approval after each section. Cover: architecture, components, data flow, error handling, testing. Be ready to go back and clarify if something doesn't make sense.

**Scope check first**: Before asking detailed questions, assess scope. If the request describes multiple independent subsystems, flag this immediately and help the user decompose into sub-projects — don't spend questions refining details of a project that needs decomposition first. Each sub-project gets its own spec → plan → implementation cycle.

**Working in existing codebases**: Explore the current structure before proposing changes. Follow existing patterns. Where existing code has problems that affect the work, include targeted improvements as part of the design — don't propose unrelated refactoring.

**Design for isolation and clarity**: Break the system into smaller units that each have one clear purpose, communicate through well-defined interfaces, and can be understood and tested independently. For each unit, you should be able to answer: what does it do, how do you use it, and what does it depend on? Smaller, well-bounded units are easier to reason about.

## After the Design

1. **Write design doc** — save to `docs/YYYY-MM-DD-<topic>-design.md` and commit
2. **Spec self-review** (see below) — fix issues inline
3. **User reviews spec** — ask user to review the spec file before proceeding:
   > "Spec written and committed to `<path>`. Please review it and let me know if you want to make any changes before we start writing out the implementation plan."
   
   Wait for the user's response. If they request changes, make them and re-run the spec review. Only proceed once the user approves.
4. **Transition to implementation** — invoke `writing-plans` skill to create implementation plan. Do NOT invoke any other skill.

## Spec Self-Review

After writing the spec document, look at it with fresh eyes:

### 文档正确性检查

1. **Placeholder scan:** Any "TBD", "TODO", incomplete sections, or vague requirements? Fix them.
2. **Internal consistency:** Do any sections contradict each other? Does the architecture match the feature descriptions?
3. **Scope check:** Is this focused enough for a single implementation plan, or does it need decomposition?
4. **Ambiguity check:** Could any requirement be interpreted two different ways? If so, pick one and make it explicit.

### 代码事实核查(必做)

方案中所有引用代码的声明**必须实际验证**,不允许凭参数记忆断言:

5. **现状分析表**:方案开头必须含"现状分析"表,每行一个组件,列含:位置(文件:行号)/ 现状 / 验证标记。未验证标 ⚠️,已验证标 ✅
6. **签名/可见性/依赖验证**:函数签名(async/sync、&self/&mut self)、常量可见性(pub/private)、crate 依赖关系 —— 必须通过 Read/Grep 核查,不能假设
7. **行号附注**:引用代码位置必须附行号或文件路径,不允许只说"在 xxx 模块中"

### 实现可行性推演(必做,逐项回答)

以下 9 项必须逐项推演,结果记入方案的"实现可行性评审"章节:

8. **签名兼容性**:新调用的函数签名与调用点上下文兼容吗?(async/sync、&self/&mut self、生命周期)
9. **参数来源**:每个函数参数从哪来?调用方签名里有吗?
10. **数据传递链**:数据从产生点到消费点,中间每个层级都传递了吗?会丢失吗?
11. **判定优先级**:多个判定条件共存时,顺序对吗?漏判成本 vs 误判成本谁高?
12. **retry/重入**:被重复调用时成本可控吗?有缓存/幂等吗?
13. **冲突处理**:外部输入与内部状态冲突时,谁优先?优先规则安全吗?
14. **与现有系统重叠**:新机制与现有机制职责重叠吗?
15. **失败路径**:每个外部依赖失败时,系统行为是什么?降级还是阻塞?
16. **成本估算**:行数估算含 prompt 工程/错误处理/边界 case 吗?

Fix any issues inline. No need to re-review — just fix and move on.

## Visual Companion

A browser-based companion for showing mockups, diagrams, and visual options. **Only offer when the task involves UI/visual questions** — for backend/toolchain/algorithm tasks, skip this entirely.

If visual questions are ahead, offer it once as its own message (no other content):
> "Some of what we're working on might be easier to explain if I can show it to you in a web browser. I can put together mockups, diagrams, comparisons, and other visuals as we go. Want to try it?"

Per-question decision: use browser for visual content (mockups, layouts, diagrams), terminal for text content (requirements, tradeoffs, scope). If accepted, read `visual-companion.md` for detailed guide.
