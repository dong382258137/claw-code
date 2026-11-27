# IM 桥接并入 WebUI 与 BOT 模式：可行性与方案调研

> 调研日期：2026-11-27。只做分析与方案，不改代码。
> 结论基于仓库当前实现（`im-bridge` 4,509 行、`claw-server` 的 serve/scheduler/LiveCli）与公开文档/仓库。

---

## 0. 结论速览

1. **并入 WebUI 在架构上成立，但不能"把 im-bridge 的 agent 路径搬进 serve"**。两者是两套执行内核：im-bridge 是「每聊天一个 ACP 线程 + `BridgeApiClient`」，serve 是「每会话一个 JSONL + `LiveCli` + `Scheduler`」。正确做法是**只保留 im-bridge 的传输层（飞书/企微连接器与 webhook 验签），执行侧全部改用 serve 的调度器与会话**，然后删掉 `SessionManager`、ACP 握手、`BridgeApiClient` 这条旧链路。
2. **主要障碍不在端口/框架（都是 axum 0.8 + tokio），而在四件事**：鉴权模型相反（Web 用 URL token，IM 用平台签名）、审批模型缺失（IM 没有交互式审批界面）、并发模型冲突（im-bridge 每聊天无限线程，serve 全局 4 / 每工作区 1）、会话身份不同（`ChatKey{platform,chat_id}` vs `session_id` + JSONL）。这些都需要显式设计，不能靠"合并 crate"解决。
3. **BOT 模式"只调度不执行"在本仓库有现成抓手**：`LiveCli::new` 已经接受 `allowed_tools` 白名单（[app.rs#L1257-L1288](file:///d:/claw-code-src/rust/crates/claw-server/src/app.rs#L1257-L1288)），`tools` 层已有"广告=可执行"的过滤与执行期 guard（[lib.rs#L781-L913](file:///d:/claw-code-src/rust/crates/tools/src/lib.rs#L781-L913)），`runtime` 已有 `SubagentCapability` 三级能力白名单（[multi_agent/mod.rs#L68-L149](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L68-L149)）。**推荐"工具白名单 + 只读权限"双保险，而不是只靠提示词**；调度对象用"独立 worker 会话"，不要用进程内 `Agent` 线程，否则 Web 端看不见、也扛不住重启。
4. **最值得借鉴的开源项目是 DeepSeek 自己的 `dsh-superagent-prototype`**：它就是"主会话 router（小工具集）+ 每张工单一个 worker 会话 + GitHub/飞书 Source + Outbox 回复 + 授权/幂等"，与本诉求几乎一一对应。其次是 OpenClaw 的"渠道插件 → 每用户 agentId + workspace 绑定"和 Claude Code 官方的 Slack/GitHub 集成。

| 判断项 | 结论 | 置信度 |
|---|---|---|
| IM 桥接并进 `claw serve`（同一二进制/同一会话内核） | 可行，需分两阶段，先传输层后内核 | 高 |
| 保留独立 `claw-im-bridge.exe` 作为 Sidecar | 可行，但不符合"不单独进程"的诉求 | 高 |
| BOT 模式用"工具白名单 + 只读权限"实现只调度 | 可行，改动集中在 `PromptJob`/`LiveCli`/`tools` | 高 |
| 单主会话直接承接多群多人的上下文而不串 | 有风险，必须做"路由器 + 每工单会话"或"每会话信封" | 中高 |
| 直接复用 DeepSeek Harness 代码 | 不可（TS + Cordis 插件体系），只能借鉴领域模型 | 高 |

---

## 1. 仓库现状（代码事实）

### 1.1 im-bridge 的进程模型

[main.rs#L18-L167](file:///d:/claw-code-src/rust/crates/im-bridge/src/main.rs#L18-L167)：

- 独立二进制 `claw-im-bridge`，自己建多线程 tokio runtime；
- 配置固定读 `~/.claw/im-bridge.toml`（[config.rs#L110-L146](file:///d:/claw-code-src/rust/crates/im-bridge/src/config.rs#L110-L146)），必填 `[feishu]` 或 `[wecom]`；
- 自己初始化 MCP（与 CLI 同源，重复一份 `init_mcp_tools`）；
- **权限直接 `PermissionMode::Allow`**，注释写明"飞书/企微消息即视为信任用户，全部放行"（[main.rs#L127-L131](file:///d:/claw-code-src/rust/crates/im-bridge/src/main.rs#L127-L131)）；
- 未配置 `workspace_roots` 时**自动枚举所有盘符根**作为文件工具白名单（"最大权限"）（[main.rs#L93-L111](file:///d:/claw-code-src/rust/crates/im-bridge/src/main.rs#L93-L111)、[main.rs#L224-L237](file:///d:/claw-code-src/rust/crates/im-bridge/src/main.rs#L224-L237)）；
- 无交互审批界面（通道里没有 prompter）。

### 1.2 im-bridge 的会话映射

[session.rs#L46-L59](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs#L46-L59) 定义 `ChatKey { platform, chat_id }`，它是**唯一会话键**；`user_id` 只作为 `AgentEntry` 的附属字段保存（[session.rs#L87-L97](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs#L87-L97)）。

带来的直接后果：

- **同一个群里的多个用户共享一个 agent 会话**（群 `chat_id` 相同），群成员能看到彼此的上下文；
- 私聊 `chat_id` 通常是用户维度，问题不大；群聊/话题（thread）没有区分；
- 每个 `ChatKey` 首次消息时 spawn 一个独立线程 + ACP 握手（initialize → authenticate → new_session），各 agent 通知流合并到一个 channel（[session.rs#L622-L734](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs#L622-L734)）；
- 空闲 30 分钟后清理（[session.rs#L788-L824](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs#L788-L824)）；
- 持久化只存元数据到 `~/.claw/im-bridge-sessions.json`，**ACP 会话本身不能跨重启恢复**（[persistence.rs#L1-L20](file:///d:/claw-code-src/rust/crates/im-bridge/src/persistence.rs#L1-L20)）。

传输与回复：

- axum 路由 `/health`、`/feishu/webhook`、`/wecom/webhook`、`/api/bus/*`（[server.rs#L158-L180](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L158-L180)）；
- 飞书支持 webhook 签名校验，也支持免公网的长连接 WS 模式（[server.rs#L115-L140](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L115-L140)）；
- 企微要求 5 秒内返回被动 ack，然后异步推送（[server.rs#L473-L544](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L473-L544)）；
- 回复靠 `session_id → RouteTarget` 的映射表，由通知收集器按 session 找回目标聊天（[server.rs#L384-L433](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L384-L433)）；
- 入站消息**没有 `event_id`/`message_id` 去重**（飞书 `message_id` 被提取但未用于幂等，[feishu.rs#L211-L242](file:///d:/claw-code-src/rust/crates/im-bridge/src/connectors/feishu.rs#L211-L242)）；平台重试会重复执行。

### 1.3 serve 的会话/调度模型

[serve.rs#L52-L83](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L52-L83) 的 `AppState`：

- 单二进制 `claw serve`，单端口（默认 4317），**所有 UI/API 请求用 URL 里的 `t` token 鉴权**（[serve.rs#L117-L120](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L117-L120)）；
- 会话 = `<workspace>/.claw/sessions/<fingerprint>/<id>.jsonl`，可恢复、可导出、可压缩；
- `Scheduler`：全局并发 4、每工作区 1、每会话排队 5（[scheduler.rs#L31-L92](file:///d:/claw-code-src/rust/crates/claw-server/src/serve/scheduler.rs#L31-L92)）；
- 每会话一个常驻 worker 线程串行消费队列，跨轮复用 `LiveCli`（[serve.rs#L2201-L2328](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2201-L2328)、[serve.rs#L2477-L2510](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2477-L2510)）；
- 每会话独立事件流 `EventHub`（broadcast + seq + reset），SSE 输出（[serve.rs#L2788-L2824](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2788-L2824)）；
- 提交一轮：`POST /api/session/{id}/prompt` → `PromptJob` → `scheduler.enqueue` → 202 + 排队位次（[serve.rs#L2716-L2786](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2716-L2786)）；
- 审批：`WebPermissionPrompter` 通过 SSE 等 Web 端批准，**120 秒超时即拒绝**（[serve.rs#L2085-L2195](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2085-L2195)）；
- 看门狗：900 秒无流式事件则中止并回收槽位（[serve.rs#L2222-L2268](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2222-L2268)）；
- 已有"外部事件驱动会话"的先例：`cron_runner` 每 15 秒把到期的 cron 封装成 `PromptJob` 入队，首次执行时创建专属会话（[cron_runner.rs#L1-L120](file:///d:/claw-code-src/rust/crates/claw-server/src/serve/cron_runner.rs#L1-L120)）。**IM 接入可以直接复刻这个模式，不需要新的执行通道。**

### 1.4 已经存在、可直接复用的能力

| 能力 | 位置 | 对 BOT 模式的意义 |
|---|---|---|
| 会话级工具白名单 | `LiveCli::new(..., allowed_tools, ...)`（[app.rs#L1257-L1288](file:///d:/claw-code-src/rust/crates/claw-server/src/app.rs#L1257-L1288)），但 serve 目前固定传 `None`（[serve.rs#L2307-L2315](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2307-L2315)） | 只调度不执行的**第一道闸** |
| 白名单的双向过滤 | `GlobalToolRegistry::definitions(allowed)` + `execute_tool` 校验（[lib.rs#L781-L913](file:///d:/claw-code-src/rust/crates/tools/src/lib.rs#L781-L913)） | 防止"广告里有、执行时也能跑" |
| 子代理三级能力 | `Analyze / ReadOnly / Execute` 及其 `allowed_tools()`（[multi_agent/mod.rs#L68-L149](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L68-L149)） | 可定义 `Router` 这一级"无写工具"能力 |
| 文件化 agent 定义 | `.claw/agents/<type>.md` 的 frontmatter `tools:` 可整体覆盖内置白名单（[commands/lib.rs#L3925-L4050](file:///d:/claw-code-src/rust/crates/commands/src/lib.rs#L3925-L4050)） | 不改代码就能新增 `bot-router` / `bot-worker` 预设 |
| 子代理派发 | `Agent` 工具 → 独立线程 + 独立 manifest（[tools/lib.rs#L6895-L7002](file:///d:/claw-code-src/rust/crates/tools/src/lib.rs#L6895-L7002)、`spawn.rs` 为进程内线程 + LocalSet，[spawn.rs#L1-L52](file:///d:/claw-code-src/rust/crates/claw-shell/src/spawn.rs#L1-L52)） | 适合"工单内部"的子任务，不适合作为顶层调度对象 |
| DAG 并行 + 文件锁 + handoff | `runtime/src/multi_agent/dag/*`、`file_guard.rs`、`handoff.rs` | worker 内部并行与写冲突规避 |
| 任务注册表 | `TaskCreate/TaskList/TaskOutput/TaskStop/TaskUpdate/RunTaskPacket`（[tools/lib.rs#L1715-L1815](file:///d:/claw-code-src/rust/crates/tools/src/lib.rs#L1715-L1815)） | 进程内后台任务，**不持久、不跨重启**，不能当工单存储 |
| 跨进程会话总线 | `SessionBus`，`PeerKind::{Main,Subagent,Ide,Im}`，默认 `Main→*` 放行、`Im/Ide→*` 拒绝（[session_bus.rs#L42-L60](file:///d:/claw-code-src/rust/crates/runtime/src/session_bus.rs#L42-L60)、[#L304-L312](file:///d:/claw-code-src/rust/crates/runtime/src/session_bus.rs#L304-L312)） | 合并后 serve 可注册为 `Main` peer，IM 频道注册为 `Im` peer，等价于 im-bridge 现在的 hub 能力 |
| 注意：serve 当前未注册任何 bus peer | 在 `claw-server/src` 中检索不到 `global_session_bus` | 合并后需要补这一步，才能取代 im-bridge 的 `/api/bus/*` |

另有一份 2026-11-25 的瘦身计划明确写着 **"im-bridge 独立二进制（保留，不依赖主二进制）"**（[2026-11-25-web-first-slimming-performance-plan.md#L162-L165](file:///d:/claw-code-src/docs/2026-11-25-web-first-slimming-performance-plan.md#L162-L165)）。本次诉求与该计划的方向相反，落地时需要同步更新那份文档，否则两份设计会互相矛盾。

---

## 2. 并入 WebUI 的可行性

### 2.1 三种落法

| 方案 | 做法 | 改动量 | 是否满足"不单独进程" | 风险 |
|---|---|---|---|---|
| A. 传输层 Sidecar（过渡） | 保留 `claw-im-bridge`，但删掉内部 agent；收到消息后调用 serve 的 `POST /api/session/{id}/prompt`，订阅 SSE 回帖 | 小 | 否（仍是第二个进程） | 低，可先验证路由/审批模型 |
| B. 进程内模块（目标态） | 把连接器搬进 `claw-server`（或抽成 `im-connectors` 库），与 serve 共用 tokio runtime、`Scheduler`、`EventHub`、JSONL 会话；删除 `SessionManager`/ACP/`BridgeApiClient` | 中 | 是 | 中，涉及删除旧链路与配置迁移 |
| C. 维持现状 + Session Bus | 两个进程通过 `.claw/bus` 文件队列互通 | 极小 | 否 | 低，但不解决诉求 |

**推荐 A → B**：先让 im-bridge 变成纯传输层（去掉第二套执行内核），验证 IM 侧的审批、去重、并发；再把传输层折叠进 serve，删除独立二进制。这样每一步都能单独回归，不会一次性改动执行内核。

### 2.2 主要工程障碍（按严重度排序）

1. **鉴权模型相反**
   - serve：单 token，所有路由都 `authorize(&state, &auth)`；token 在 URL 里（[serve.rs#L117-L120](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L117-L120)）。
   - IM：飞书 `X-Lark-Signature` + timestamp + nonce，企微 `msg_signature` + AES 解密（[server.rs#L335-L362](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L335-L362)、[wecom_crypto.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/connectors/wecom_crypto.rs)）。
   - 合并后 IM 回调路由必须**绕过 token、只走平台验签**，并且不能把 Web token 泄露到 IM 消息或日志。这是"路由分组"问题：`/feishu/*`、`/wecom/*` 一组（验签），其余一组（token）。
2. **审批模型缺失**
   - serve 的越权工具调用会阻塞 120 秒等 Web 端点击；IM 频道里没有人能点。
   - 合并后如果 IM 会话沿用 `workspace-write`，任何写操作都会卡住 120 秒再被拒。
   - 必须选择：IM 会话默认 `read-only`，或实现"IM 内回复 yes/no 审批"，或把审批转发到 Web 端（Bot 会阻塞）。这是产品决策，不是纯技术问题。
3. **并发模型冲突**
   - im-bridge：每个聊天一个线程，无上限（[session.rs#L8-L23](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs#L8-L23)）。
   - serve：全局 4、每工作区 1、每会话排队 5（[scheduler.rs#L31-L92](file:///d:/claw-code-src/rust/crates/claw-server/src/serve/scheduler.rs#L31-L92)）。
   - BOT 模式会产生"路由器 1 轮 + N 个 worker"的扇出，**很容易吃满全局 4**，把 Web 用户自己的对话挤成排队。需要按 profile 分池或预留槽位（见 §3.4）。
4. **会话身份与持久化不同**
   - im-bridge 只有元数据持久化，重启即丢上下文；serve 是完整 JSONL，可恢复。
   - 合并后需要一张"IM 会话/工单 → session_id → 回复地址"的映射表，且要持久化（现在 `session_router` 是内存 `HashMap`，[server.rs#L53-L58](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L53-L58)）。
5. **流式回复 vs IM 消息形态**
   - serve 输出 token 级 delta；im-bridge 目前是整段 `send_text_message`。飞书可以用交互卡片持续更新，企微智能机器人基本只能"先 ack、后整段推送"。
   - 需要一层"delta 聚合 + 节流 + 平台适配"，否则要么刷屏，要么最后一次性吐出一大段。
6. **配置与密钥**
   - 现在是 `~/.claw/im-bridge.toml` 明文存 `app_secret` / `encoding_aes_key`（[config.rs#L70-L108](file:///d:/claw-code-src/rust/crates/im-bridge/src/config.rs#L70-L108)）。
   - 合并后建议：非敏感项进工作区 `.claw.json` 的 `im` 段，密钥进 `.claw/im.secrets.json`（gitignore）或系统凭据库；同时保留旧 toml 的迁移读取。
7. **部署形态**
   - 飞书 WS 长连接不需要公网；企微回调需要公网可达（或内网穿透）。合并成一个进程后，仍然只有**一个监听端口**，但企微回调路径必须能从公网进来，这会影响 `127.0.0.1` 绑定策略（现在固定 `Ipv4Addr::LOCALHOST`，[serve.rs#L1227](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L1227)）。
8. **热升级/重启**
   - serve 支持热升级重启并复用端口/token；IM 长连接必须重连，进行中的 turn 会丢失。需要"重启后把 running 工单标成 interrupted 并恢复"的语义（见 §3.6）。
9. **工作区语义**
   - im-bridge 默认"全盘最大权限"；serve 是项目工作区。Bot 接入后必须**绑定到明确工作区**，不能沿用盘符根枚举，否则任意群成员可读写整机。

### 2.3 建议的目标形态

```
claw serve（单进程、单端口）
├── http/        token 鉴权路由（现有 Web UI/API）
│   └── im/      平台验签路由（/feishu/webhook、/wecom/webhook、/feishu/ws）
├── service/     Scheduler（profile 分池）+ cron + bot 路由器 + outbox
├── core/        LiveCli（现有执行内核）
└── infra/       会话索引、IM 映射表、密钥、热升级
```

- 连接器保留为**纯传输**：收消息 → 标准化 `InboundEvent`；发消息 → `OutboundMessage`。不持有 agent、不持有会话。
- `im-bridge` crate 最终只保留 `connectors`（可改名 `im-connectors`），`session.rs`/`response.rs`/`api_adapter.rs` 删除。
- 落地时同步修订瘦身计划中"im-bridge 独立二进制"的表述。

---

## 3. BOT 模式方案

### 3.1 职责切分：路由器会话 + 工单会话

不要用"一个会话既统筹又干活"。推荐三层：

| 角色 | 数量 | 工具集 | 权限 | 职责 |
|---|---|---|---|---|
| Router（bot 主会话） | 每工作区 1 个（可每 bot 账号 1 个） | 只读工具 + 任务/派发工具 + 回复工具 | `read-only` | 读 IM 事件、判断接不接、拆工单、派发、汇总、回复 |
| Worker（工单会话） | 每工单 1 个 | 标准执行工具（bash/edit/write/…）+ 汇报工具 | `workspace-write`（默认） | 真正改代码、跑命令、产出 |
| Subagent（工单内） | 0..N | 现有 `Agent`/DAG | 继承 worker | 工单内部的并行子任务 |

关键点：**Router 不持有任何一个 worker 的上下文**。它只持有"工单账本"（task_id、来源会话、状态、摘要、产物路径）。这样：

- 多人/多群不会串上下文：每个 worker 只服务一张工单；
- Router 的上下文不会因为某个 worker 的长输出而爆炸；
- Worker 可以独立重启、独立审计、独立展示在 Web 会话列表里。

这与 DeepSeek `dsh-superagent` 的 "main Session uses `dsh-superagent-router`" / "Each Ticket gets an Executor Session named `worker-<ticket id>`" 完全同构（见 §4）。

### 3.2 "只调度不执行"的四层约束

只靠系统提示词说"你不要改代码"是不可靠的。建议四层，从外到内：

1. **会话预设层**：新增 `.claw/agents/bot-router.md`，frontmatter `tools:` 只列白名单。仓库已支持文件化覆盖（[commands/lib.rs#L3925-L4050](file:///d:/claw-code-src/rust/crates/commands/src/lib.rs#L3925-L4050)），零代码即可先验证。
2. **工具广告层**：`LiveCli::new` 传入 `allowed_tools`，`build_runtime`/`definitions()` 只暴露白名单工具。serve 现在固定传 `None`（[serve.rs#L2307-L2315](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2307-L2315)），需要把 `PromptJob` 里的 profile 透传进来。
3. **执行守卫层**：`execute_tool` 对不在白名单的调用直接报错，防止模型幻觉出工具名（现有 `SubagentToolExecutor` 就是这个模式，[lib.rs#L8530-L8560](file:///d:/claw-code-src/rust/crates/tools/src/lib.rs#L8530-L8560)）。
4. **权限层**：Router 会话用 `PermissionMode::ReadOnly`。即使前三层被绕过，写工具/`bash` 仍然会被策略拒绝。这是最后一道保险。

Router 白名单建议（最小集）：

```
read_file, glob_search, grep_search, repomap, WebSearch, WebFetch,
ToolSearch, Skill, TodoWrite,
TaskCreate/TaskGet/TaskList/TaskStop/TaskUpdate/TaskOutput,   // 或新的 DispatchTask
SendUserMessage                                                // 回帖（见 §3.5）
```

**必须拒绝**：`bash`、`PowerShell`、`write_file`、`edit_file`、`NotebookEdit`、`Agent`、`RunTaskPacket`。
其中 `Agent`/`RunTaskPacket` 也要拒绝，因为它们是进程内线程，绕过工单账本、Web 不可见、重启即丢。

### 3.3 派发工具：复用还是新增

现有 `TaskCreate` 是"进程内后台任务"，不持久、不跨重启（[tools/lib.rs#L1715-L1815](file:///d:/claw-code-src/rust/crates/tools/src/lib.rs#L1715-L1815)），不适合做工单。

建议新增一个薄工具 `DispatchTask`（名字可再议），语义：

```
DispatchTask {
  title: string,
  brief: string,            // 给 worker 的任务书
  workspace_dir?: string,   // 默认 <workspace>/.claw/bot/work/<task_id>
  isolation?: "shared" | "worktree",
  priority?: "normal" | "high"
}
→ { task_id, session_id, state: "queued" }
```

实现上只做三件事：

1. 在工单表里落一行（持久化）；
2. 创建一个 serve 会话（复用 `create_session_file`，参考 cron 的专属会话创建，[cron_runner.rs#L1-L28](file:///d:/claw-code-src/rust/crates/claw-server/src/serve/cron_runner.rs#L1-L28)）；
3. 把 `PromptJob { profile: Worker, ... }` 投进 `Scheduler`。

不要在这里做"直接调用 LLM"或"spawn 线程"，否则又回到不可见的执行路径。

### 3.4 与调度器、并发槽位、生命周期的配合

**并发**

- `PromptJob` 增加 `profile: AgentProfile { User, Router, Worker }`；
- `Scheduler` 增加按 profile 的 gate：
  - `User`（Web 对话）：保持全局 4；
  - `Router`：1–2，且**不占用** User 的槽位；
  - `Worker`：默认 4（可配），单独计数；
- 预留策略：User 的请求永远优先拿到槽位；Router 的"人在等回复"轮次可以插队（参考 dsh "one extra slot for a person's message"）。
- 每工作区 1 的限制对 Worker 会形成串行瓶颈：要么给 Worker 单独的工作区 gate（默认 N），要么每个工单一个 git worktree（仓库已有 worktree 模式和文件锁）。

**生命周期**

- Router 会话常驻（空闲 90 秒被回收也无妨，JSONL 还在，下一轮 `resume_from_path` 重建）。
- Worker 会话：`DispatchTask` 时创建，工单结束（success/failed/cancelled）后保留 JSONL 供审计，worker 线程按现有 `WORKER_IDLE_TIMEOUT_MS` 退出。
- 重启：启动时把 `running` 工单标为 `interrupted`，并给对应 worker 投一条"恢复说明"（dsh 的 recovery note 模式），避免重复执行外部副作用。
- 看门狗：Worker 长命令期间靠工具结果刷新心跳；`bash` 已支持取消（[serve.rs#L2239-L2245](file:///d:/claw-code-src/rust/crates/claw-server/src/serve.rs#L2239-L2245)），保持现状即可。

**队列与背压**

- Router 每会话排队 5 对 IM 洪峰不够用：入站先按 `(platform, chat_id, thread)` 做**去抖/合并**，同一话题 1–2 秒内的多条消息合并成一次 Router 轮次；超限时回一条 IM 提示，而不是把 HTTP 429 抛给平台。
- 平台侧有限流（飞书/企微都有），出站要有令牌桶与重试；dsh 的 outbox "5 秒起、翻倍到 300 秒"是可直接抄的策略。

### 3.5 IM 路由与"不串会话"

**身份键**

```
chat_scope = platform + ":" + chat_id + (":" + thread_root_id if 有话题)
task_key    = task_id（全局唯一）
session_key = "bot-router:" + workspace           // 路由器
worker_key  = "task:" + task_id                   // worker
```

- **禁止**用 `user_id` 当会话键（群里会拆成 N 个会话，且机器人无法统一统筹）；
- **禁止**用 `chat_id` 直接当 worker 会话键（同一群多个工单会串）；
- 群聊必须区分话题/回复串（飞书 `thread_id` / 根消息 id），否则同一群的两件事会进同一个 worker。

**入站信封**

每条 IM 消息标准化为：

```json
{
  "platform": "feishu|wecom",
  "event_id": "…",           // 幂等键
  "chat_id": "…",
  "thread_id": "…",
  "user_id": "…",
  "text": "…",
  "ts": 0,
  "reply_to": { "platform": "…", "chat_id": "…", "thread_id": "…" }
}
```

Router 的每一轮提示词都带这个信封，回复必须显式带 `reply_to`，不能依赖"上一条来自哪个聊天"。

**幂等**

- 建一张 `processed_events(platform, event_id, ts)` 表，TTL 3–7 天；
- 飞书/企微都会重试；现在 im-bridge 没做去重，合并时必须补。

**授权**

- `allowed_users` / `allowed_chats` 白名单，默认拒绝群聊；
- 谁能"派活"、谁能"批准"、谁能"停止"分开授权；
- 现在的 `PermissionMode::Allow` + 全盘根白名单必须收敛，否则任意群成员等于拿到整机 shell。

**回复通道**

- `Outbox` 表：`(task_id, reply_to, kind: ack|progress|result|attention, text, state)`；
- Worker 的产出先到 Router（或直接到 Outbox），由 Outbox 统一发到原始会话；
- 同一 `(task, conversation)` 的消息严格保序（先 ack 后结果）；
- 飞书可用交互卡片做"进行中/完成"状态更新；企微只发整段，避免刷屏。

### 3.6 数据模型（最小集）

```
inbound_event(event_id PK, platform, chat_id, thread_id, user_id, text, ts, disposition)
task(task_id PK, origin_platform, origin_chat, origin_thread, title, state, session_id,
     workspace_dir, created_at, updated_at, last_report)
outbox(id PK, task_id, platform, chat_id, thread_id, kind, text, state, attempts, next_at)
bot_config(bot_id, platform, workspace, app_id/corp_id, secret_ref, allow_rules)
```

存储建议：`<workspace>/.claw/bot.db`（SQLite，仓库已有 SQLite 使用先例：`history.db`、`sessions.db`）。不要用内存 `HashMap`（现在 `session_router` 就是内存的，重启即丢，[server.rs#L53-L58](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L53-L58)）。

### 3.7 分阶段实施

| 阶段 | 内容 | 验收 |
|---|---|---|
| P0 | 只做传输层收敛：im-bridge 去掉 agent，改为调用 serve HTTP；补 `event_id` 去重、白名单、IM 侧只读权限 | 两个平台各跑通"消息→会话→回复"；重复 webhook 不重复执行 |
| P1 | 进程内合并：连接器进 `claw-server`，删除 `SessionManager`/ACP/`BridgeApiClient`；serve 注册 `Main` bus peer | 单进程单端口；Web 与 IM 同时可用；旧 exe 不再构建 |
| P2 | Router 预设 + `DispatchTask` + 工单表 + Outbox；`PromptJob` 增加 profile 与分池 gate | Router 无写工具（执行期被拒）；工单可并行；Web 能看到 worker 会话 |
| P3 | 恢复/幂等/审批：重启中断恢复、IM 内审批或审批转发、卡片进度 | 重启不重复副作用；IM 写操作有明确授权路径 |

---

## 4. 开源借鉴

### 4.1 总览

| 项目 | 官方 IM 接入 | 实际做法 | 可借鉴点 |
|---|---|---|---|
| **DeepSeek Harness** | 否（Web/桌面） | Cordis 插件内核，"一切皆插件"；append-only 会话日志；Standard/Code/Minimal/Creator 多模式 | 能力插件化、会话可回放、模式即预设 |
| **dsh-superagent-prototype** | 是（飞书 + GitHub） | 主会话 `router` 预设（小工具集）+ 每工单 `worker-<id>` 会话 + Source/Thread/Trigger/Ticket/Assignment/Outbox + 授权/幂等/恢复 | **与本次诉求最接近的完整范式** |
| **Claude Code** | 官方 Slack（beta）+ 官方 GitHub Actions；无飞书官方 | Slack 里 @Claude 识别为编码任务 → 自动创建 Claude Code 云会话；GitHub 里 @claude → 跑 Action；自建靠 Agent SDK | "IM 只做入口，执行在独立会话"；官方不做飞书，社区桥接 |
| **Codex** | 无官方 IM | `codex exec` 非交互；官方 App Server 是双向 JSON-RPC 嵌入面，Python/TS SDK 都是它的客户端 | 把"执行内核"做成 app-server，IM/Slack/IDE 都只是客户端 |
| **opencode** | 无官方 IM | 官方 server + OpenAPI 3.1 + SDK，TUI 只是客户端；IM 靠社区（Telegram/Discord 插件） | 单 server 多客户端；社区插件承载渠道 |
| **Trae** | 未见官方 IM | 开源 `bytedance/trae-agent`（SWE-bench 用 CLI agent）；IDE 侧 SOLO | 执行内核与 IDE 分离；IM 不在其范围 |
| **OpenClaw** | 是（20+ 渠道，含飞书） | 单 Gateway 进程 + 渠道插件；飞书渠道按 `user_open_id` 生成 `agentId`、建 workspace、注册绑定 | **每用户/每会话独立 agent + 工作区模板**，防串会话 |
| **ACP（Zed）** | 否 | 编辑器 ↔ agent 的 JSON-RPC 标准（"LSP for agents"） | 传输与执行解耦；本仓库 `claw-acp` 已在用 |

### 4.2 重点拆解

**dsh-superagent-prototype（最值得抄）**

- 领域模型：`Source → Trigger → Thread → Eval → Ticket → Assignment → Executor`，回复走 `Channel/Outbox`（001 Domain）。
- 主会话只做路由：`dsh-superagent-router` 预设，"smaller tool set for routing"（003 Execution）。
- 每个工单一个执行会话：`worker-<ticket id>`，独立目录（默认 `<stateDir>/work`），可配 `git worktree` 准备命令（003）。
- 并发：worker slot 受配置上限约束，**额外留一个 slot 给人的消息/状态查询**，保证人不会被新任务饿死（003 Concurrency）。
- 恢复：重启时把运行中的 Assignment 标为中断，带 recovery note 重启；未读消息重投；交付失败 2 秒起指数退避（003 Failure and recovery）。
- 幂等：Trigger 存一次，接受两次只生成一张 Ticket（001/007）。
- 授权：谁能派活、谁能停止、哪些规则允许主动工作，单独成文（002 Authorization）。
- 渠道：GitHub + 飞书；worker 通过 `lark-cli` 回飞书，环境变量里带 `DSH_SUPERAGENT_FEISHU_BOT/CHAT`（003 Execution variables）。

**OpenClaw（防串会话的直接参考）**

- 单 Gateway 进程 + 渠道插件（Discord/Slack/Telegram/飞书/…）。
- 飞书渠道：默认 `agentId = feishu-{user_open_id}`，在 `workspaceTemplate` 下为该用户建新工作区、注册 Agent 并绑定。
- 即"IM 身份 → 独立 agent + 独立工作区"是成熟做法；但它的目标是"个人助理"，不是"一个主脑统筹多个群"，所以 BOT 模式仍需 Router + Worker 两层。

**Claude Code（官方边界）**

- Slack 集成是"研究预览"：@Claude 判断是否编码任务，是则创建 Claude Code 云会话，并拉取最近频道/线程上下文。
- GitHub Actions 是仓库内 @claude 触发。
- 飞书没有官方支持，社区桥接（`lark-channel-bridge` / `feishu-claude-code-bridge` / `feishu-bridge`）都是"把 IM 事件转成 Claude Code SDK/CLI 调用"。
- 结论：主流厂商的官方做法是**"IM 入口 + 云端/本地独立会话"**，而不是让 bot 在同一个对话里既聊又改。

**Codex（嵌入面设计）**

- `codex exec` 解决脚本化；官方 App Server 是双向 JSON-RPC，SDK 只是它的客户端。
- 对本项目的启发：如果 IM 只是"另一个客户端"，那它应该走 serve 的 API，而不是自己再长一套 agent。

**opencode（多客户端）**

- 官方 server + OpenAPI + SDK；TUI 只是客户端。
- IM 接入全部由社区完成（`grinev/opencode-telegram-bot` 1.4k stars、`jshsakura/opencode-telegram-bot-plugin`、`code-xhyun/disunday` Discord）。
- 结论：把"渠道"留给插件/连接器，把"会话与执行"留给 server，是被验证过的分工。

**Trae**

- `bytedance/trae-agent` 是开源 CLI agent（论文称 SWE-bench Verified 75.20% Pass@1），与 IDE 的 SOLO 是两条线；未检索到官方 IM bot。
- 对本项目没有直接可抄的 IM 设计，但印证"agent 内核 + 多前端"的分离。

---

## 5. DeepSeek 的开源 agent 框架

有，而且不止一个：

1. **DeepSeek Harness**（`deepseek-ai/deepseek-harness`）
   - 定位：agent harness，"Agent = Model + Harness"。
   - 架构：Cordis 插件内核（`cordiverse/cordis`），模型/工具/技能/会话/沙箱/存储/循环/调度/UI 全是插件；配置组合，不改源码。
   - 运行形态：`npx @deepseek-ai/dsh web` 或桌面应用；会话是 append-only 日志，支持 resume/fork/search/replay。
   - 模式：Standard（全工具）、Code（代码编排多轮工具）、Minimal（仅 shell + 编辑器）、Creator（运行时检查与预设编写）。
2. **dsh-superagent-prototype**（`deepseek-ai/dsh-superagent-prototype`）
   - 定位：面向单个 GitHub 仓库的"主动型编码 agent"，以 Harness 插件包形式分发。
   - 能力：监听 GitHub，可选飞书；有人派活就开一张工单，在**自己的 agent 会话和目录**里干活，回到原会话回复（例如 PR）。
   - 明确声明是研究原型、非官方产品、MIT。
   - 与本次诉求的对应关系：Source=飞书/企微，Router=主会话，Ticket=工单，Executor Session=worker 会话，Channel/Outbox=IM 回复，Policy=授权。

**能不能借鉴**：领域模型、预设分层、并发槽位、幂等/恢复策略可以**直接照搬设计**；代码不能直接搬（TypeScript + Cordis 插件 + Harness 运行时），需要按本仓库的 Rust `runtime` + `Scheduler` 重写。

---

## 6. 参考清单

**DeepSeek**

- DeepSeek Harness 官网（英文）：https://www.deepseek.com/harness/en/
- DeepSeek Harness 仓库：https://github.com/deepseek-ai/deepseek-harness
- DeepSeek Harness 开发者文档：https://deepseek-harness.github.io/deepseek-harness/en/guide/quickstart
- dsh-superagent-prototype：https://github.com/deepseek-ai/dsh-superagent-prototype
  - 领域模型：https://github.com/deepseek-ai/dsh-superagent-prototype/blob/main/docs/001-domain.md
  - 执行与并发：https://github.com/deepseek-ai/dsh-superagent-prototype/blob/main/docs/003-execution.md
  - 架构：https://github.com/deepseek-ai/dsh-superagent-prototype/blob/main/docs/007-architecture.md
- Cordis 内核：https://github.com/cordiverse/cordis

**Claude Code**

- Claude Code in Slack（官方，beta）：https://code.claude.com/docs/en/slack
- Claude Code GitHub Actions（官方）：https://code.claude.com/docs/en/github-actions
- Claude Agent SDK（自建 agent/bot）：https://code.claude.com/docs/en/agent-sdk/overview
- Claude Code 与 Slack 发布说明：https://claude.com/blog/claude-code-and-slack
- 社区飞书桥接（npm `lark-channel-bridge`，仓库 `zarazhangrui/feishu-claude-code-bridge`）：https://www.npmjs.com/package/lark-channel-bridge
- 社区飞书桥接（`feir/feishu-bridge`）：https://pypi.org/project/feishu-bridge/

**Codex**

- 非交互模式 `codex exec`：https://developers.openai.com/codex/noninteractive/
- Codex App Server（双向 JSON-RPC）：https://openai.com/index/unlocking-the-codex-harness/
- Codex Python SDK（包装 app-server）：https://codex.danielvaughan.com/2026/03/30/codex-python-sdk-embedding-agents/

**opencode**

- Server（OpenAPI 3.1 + 多客户端）：https://opencode.ai/docs/server
- SDK：https://dev.opencode.ai/docs/sdk/
- 社区 Telegram bot：https://github.com/grinev/opencode-telegram-bot
- 社区 Telegram 插件：https://www.npmjs.com/package/@jshsakura/opencode-telegram-bot-plugin
- 社区 Discord：https://github.com/code-xhyun/disunday

**Trae**

- Trae Agent（开源）：https://github.com/bytedance/trae-agent
- 论文：https://arxiv.org/pdf/2507.23370

**OpenClaw**

- 文档（中文）：https://docs.openclaw.ai/zh-CN
- 飞书渠道（每用户 agentId + workspace 绑定）：https://docs.openclaw.ai/zh-CN/channels/feishu

**协议**

- Agent Client Protocol（Zed）：https://zed.dev/acp
- ACP 外部 agent 文档：https://zed.dev/docs/ai/external-agents

---

## 7. 落到本项目的改动点（只列位置，不改代码）

| 模块 | 需要做什么 |
|---|---|
| `rust/crates/im-bridge/src/connectors/*` | 保留为纯传输层；抽成 `im-connectors` 或直接并入 `claw-server` |
| `rust/crates/im-bridge/src/session.rs` | 删除（被 `Scheduler` + `PromptJob` 取代） |
| `rust/crates/im-bridge/src/api_adapter.rs` | 删除（`BridgeApiClient` 不再需要） |
| `rust/crates/im-bridge/src/server.rs` | webhook 路由/验签并入 `claw-server/src/serve`（建议 `serve/im_routes.rs`） |
| `rust/crates/im-bridge/src/response.rs` | 改为订阅 `EventHub` + `Outbox` |
| `rust/crates/im-bridge/src/persistence.rs` | 改为 `bot.db`（工单/映射/幂等/outbox） |
| `rust/crates/im-bridge/src/tools.rs`（IM 排除工具） | 变为 Router/Worker 预设的工具裁剪 |
| `rust/crates/claw-server/src/serve/scheduler.rs` | `PromptJob` 增加 `AgentProfile`；按 profile 分池/预留槽位 |
| `rust/crates/claw-server/src/serve.rs`（`LiveCli::new` 调用点） | 透传 `allowed_tools` 与 profile；新增 IM 路由与 bot 初始化 |
| `rust/crates/claw-server/src/app.rs`（`LiveCli::new`） | 已支持 `allowed_tools`，无需改签名 |
| `rust/crates/tools/src/lib.rs` | 新增 `DispatchTask`（或等价工具）；Worker 汇报工具 |
| `rust/crates/runtime/src/multi_agent/*` | Worker 内部并行/文件锁/worktree 复用 |
| `rust/crates/runtime/src/session_bus.rs` | serve 注册 `Main` peer；IM 频道注册 `Im` peer（可选） |
| `docs/2026-11-25-web-first-slimming-performance-plan.md` | 修订"im-bridge 独立二进制（保留）"的结论 |
| `rust/crates/claw-server/web/src/panel/TasksPane.jsx` | 增加工单视图（来源会话、状态、产物） |

---

## 8. 风险与验证建议

**风险**

1. Router 被"越狱"执行写操作：必须执行期 guard + `read-only` 双保险，不能只靠 prompt。
2. 单 Router 会话被多群上下文污染：每轮都带信封、worker 隔离、必要时按 `(chat, thread)` 拆 Router 会话。
3. 并发饿死：Worker 分池 + 人类请求预留槽位，否则 Web 用户会被 bot 挤爆。
4. 重启重复副作用：`processed_events` 幂等 + 工单 `interrupted` 恢复 + 外部动作用"先查后做"。
5. 安全面扩大：去掉全盘根白名单与 `Allow`，改为工作区绑定 + 用户/群白名单 + 分角色授权。
6. 平台限制：企微 5 秒 ack、消息不可更新；飞书需公网或 WS；长文本要分片。

**建议的验证（后续实施阶段）**

- 同群两个用户同时派活：两个 worker 会话、互不可见对方上下文。
- 同一 webhook 重放 3 次：只执行 1 次。
- Router 强行调用 `write_file`/`bash`：执行期被拒，并回一条可读错误。
- 全局 4 槽位占满时，Web 用户发消息仍能插队执行。
- serve 重启时正在跑的工单：标 interrupted、不重复已完成的副作用。
- 飞书 WS 断线重连、企微回调超时重试：不丢消息、不重复回复。

---

## 9. 一句话总结

并入 WebUI 的正确姿势是**"留传输、换内核"**：把飞书/企微连接器收进 `claw-server`，执行全部交给现有 `Scheduler + LiveCli + JSONL`，删掉 im-bridge 的第二套 agent 链路。BOT 模式则用**"Router 会话（只读工具 + 派发工具）+ 每工单一 Worker 会话 + 持久化工单/Outbox"**，用工具白名单与只读权限做硬约束，而不是靠提示词。开源侧最值得对照的是 DeepSeek 自己的 `dsh-superagent-prototype`，其次是 OpenClaw 的"每用户 agentId + 工作区绑定"和 Claude Code 的"IM 入口 + 独立云会话"。
