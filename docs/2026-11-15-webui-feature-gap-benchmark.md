# WebUI 功能对标与缺口清单（2026-11-15）

## 0. 范围与方法

- 阅读对象：`rust/crates/rusty-claude-cli/web/src`（前端源码）与 `rust/crates/rusty-claude-cli/src/serve.rs`（约 4.7k 行、55 条路由），以及 `serve/scheduler.rs`、`tool_display.rs`、`runtime/conversation.rs` 中被前端依赖的部分。
- 判定依据：**只认代码实际行为**，不采信 `docs/` 里对 WebUI 的描述；"文档说已做但代码没有"或"代码有但路径不通"的地方单独标注。
- 对标对象：Claude Code、Cursor、Cline、OpenHands、Devin、Manus、ChatGPT 与 Claude 桌面端。产品能力以其公开版本的通用形态为准，未逐版本核对，个别细节见第 6 节。
- 局限：第 1–6 节是静态阅读 + 代码路径推演；**第 7 节是 2026-11-15 追加的真实界面核验**（自编译、独立端口、Playwright 实际点击），两者冲突时以第 7 节为准。

判定标签：

| 标签 | 含义 |
|---|---|
| 确实缺 | 没有任何对应能力，入口和实现都没有 |
| 有但粗糙 | 有入口、能跑通，但达不到日常顺手用 |
| 半缺 | 只做了闭环的一半（如只能丢弃不能回滚） |
| 取舍 | 有能力差异，但属于项目已明确的产品选择，不算缺陷 |

---

## 1. 现状盘点

### 1.1 已经具备的能力

| 能力 | 实现位置 | 备注 |
|---|---|---|
| 多工作区 + 会话按工作区分组 | [Sidebar.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/shell/Sidebar.jsx#L51-L65) | 工作区来自 `/api/workspaces`，侧栏可折叠 |
| 不选会话直接输入（自动建会话再发） | [App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L416-L470) | 少一步"先建任务"的管理动作 |
| 单会话 SSE 增量 + 断线补快照 | [useSessionEvents.js](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/useSessionEvents.js#L26-L143) | 重连、`done`、`reset` 后重拉完整视图 |
| 流式文本 / 思考 / 工具卡，按"轮"折叠并给过程摘要 | [Conversation.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Conversation.jsx#L121-L145)、[turns.js](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/turns.js#L78-L88) | 摘要形如"已读取文件，已搜索代码，用时 N 秒" |
| 取消（interrupt）、流错误可重试 | [App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L457-L466)、[App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L416-L454) | 错误时把原文回填输入框 |
| 权限三档 + Web 端阻塞式审批 | [Composer.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Composer.jsx#L541-L568)、[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1056-L1091) | 只读 / 工作区可写 / 完全访问 |
| 文件面板：预览文本、Markdown、图片、PDF，行区间引用 | [FilesPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/FilesPane.jsx#L76-L127) | 引用生成 `@path:12-40`，后端展开成代码块 |
| 变更面板：文件级 diff + 逐 hunk 丢弃 | [DiffPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/DiffPane.jsx#L32-L51) | 带 fingerprint 校验，防并发误丢 |
| 任务面板：目标、计划、Web 侧可编辑待办、历史 TodoWrite | [TasksPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/TasksPane.jsx#L11-L121) | Web 待办会在下一条消息中带给 agent |
| 终端（xterm + WebSocket PTY） | [TerminalPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/TerminalPane.jsx) | 支持 vim/htop 这类全屏程序 |
| 附件：上传、粘贴缓存、工作区图片 | [AttachmentsPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/AttachmentsPane.jsx) | 上传上限 10MB |
| 技能市场 + 插件市场（源管理、安装、启停、卸载、缓存索引提示） | [PluginMarketPage.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/market/PluginMarketPage.jsx) 等 | 比多数同类产品的内置市场更完整 |
| 语音输入 + 语音讨论（独立无工具端点、TTS、总结回填输入框） | [Composer.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Composer.jsx#L363-L432) | 总结只填入待编辑，不直接发送 |
| 按轮切模型 + 自定义模型名 | [Composer.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Composer.jsx#L603-L641) | 与 TopBar 共用同一设置 |
| 轮末统计：轮数、步数、tok/s、token、缓存命中率 | [Composer.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Composer.jsx#L703-L708) | 数据来自会话快照，不是实时流 |
| 每会话草稿（sessionStorage） | [Composer.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Composer.jsx#L65-L77) | 切换会话不丢正在输入的内容 |
| 面板停靠/全屏/拖宽、侧栏收起、深浅主题 | [App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L192-L262) | 布局设置持久化到服务端 |

### 1.2 界面骨架

侧栏（工作区 → 会话）＋ 顶栏（标题、chips、模型、面板开关）＋ 对话流 ＋ 输入区（附件、权限、模型、语音、统计）＋ 右侧五标签面板（文件/变更/任务/附件/终端）＋ 全屏审批弹层 ＋ 设置页 ＋ 两个市场页。路由只有 `chat / plugins / skills` 三条（[router.js](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/shared/router.js#L5-L15)）。

---

## 2. 对标矩阵

| 维度 | 本 WebUI | 成熟产品代表做法 | 判定 |
|---|---|---|---|
| 会话管理 | 工作区分组、重命名、批量删除、按标题过滤 | ChatGPT/Claude 全库搜索；Cline 任务历史；Cursor 会话列表 | 粗糙 + 缺 |
| 上下文与文件 | `@` 引用、行区间、附件面板、独立预览 | Cursor `@` 文件/符号/文档；Claude/ChatGPT 附件内联预览 | 粗糙 |
| 工具调用可视化 | 名称 + 原始 JSON + 纯文本输出 | Claude Code 分工具卡片；Cline 逐动作审批 + diff；Cursor 内联 diff | 粗糙 |
| 多任务并行 | 后端全局并发 4，界面上只跟一个会话 | Cursor 后台任务看板 + 完成通知；Devin 会话状态看板 | 缺界面 |
| 计划与进度 | 只读面板，手动刷新 | Claude Code 实时 Todo 清单；Devin 计划勾选进度；Manus 步骤进度 | 缺 |
| 产物预览 | 图片/PDF 可看，无产物概念 | Manus 产物面板；Claude artifacts 侧栏；Devin PR/预览链接 | 缺 |
| 权限确认 | 三档 + 阻塞弹窗，只有允许/拒绝 | Cline/Cursor 逐动作确认 + 自动批准白名单 + diff 预览 | 粗糙 |
| 编辑与审查 | 只能事后逐 hunk 丢弃 | Cursor/Cline 逐块接受/拒绝 + checkpoint 恢复；Claude Code `/rewind` | 半缺 |
| 上下文与成本 | 轮末 token/缓存/tok-s | Cline 上下文窗口占用条与成本；Claude Code 压缩提示 | 粗糙 |
| 搜索 | 只过滤已加载会话的标题/ID/路径 | 全库会话搜索、消息正文命中 | 缺 |
| 快捷键与命令 | 2 个快捷键、6 个面板开关斜杠命令 | 命令面板 + 全套快捷键；语义化斜杠命令 | 缺 |
| 终端 | 单 PTY，固定 serve 启动目录 | OpenHands 工作区终端；Cursor 按工作区开终端 | 缺陷 |
| 扩展生态 | 插件/技能市场 + 源管理 | Cline MCP 市场；Claude connectors | 领先 |
| 语音 | 语音输入 + 讨论 + 播报 | ChatGPT 语音模式 | 不弱 |
| 多工作区 | 原生目录选择 + 会话分组 | Cursor 多根工作区 | 不弱 |

---

## 3. 缺口清单（按对日常使用的影响排序）

每条包含：现状（代码证据）→ 成熟产品怎么做 → 补上要动哪里 → 标签与不确定点。改动量用"动到的层"描述，不给工期。

### 3.1 多任务并行：底座有，界面完全没有（影响最大）

- **现状**：调度器默认全局并发 4、**每工作区并发 1**（[scheduler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve/scheduler.rs#L78-L83)），后端允许 4 个会话同时跑。但前端只订阅"当前会话"一条事件流（[useSessionEvents.js](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/useSessionEvents.js#L26-L82)），侧栏只显示标题和相对时间（[Sidebar.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/shell/Sidebar.jsx#L203)），没有运行/排队标记；换个会话就看不到原来的进度；也没有任何完成通知（前端无 `Notification`、无标题栏提醒）。会话状态接口其实返回 `state/position/queued`（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1755-L1770)），但前端只取了 `running` 一个布尔值轮询（[App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L161-L169)）。
- **成熟做法**：Cursor 有后台 agent 看板，任务完成弹通知；Devin 的会话列表直接显示"运行中/等你输入/已阻塞"；Claude Code 多会话并行时列表带状态与未读。
- **补上要动**：① `api_sessions` 返回每个会话的 `state/position`（复用 `scheduler.status`）；② 前端侧栏加状态点、全局"N 个任务在跑"入口、完成时 `Notification`（需用户授权）；③ 非当前会话的进度要么轮询 status，要么在 `EventHub` 上再加一条"全局事件流"（现在只有按会话订阅，[scheduler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve/scheduler.rs#L823-L860)）。
- **标签**：确实缺（界面层）。**不确定**：每工作区并发 1 是否刻意为之（避免同一仓库并发改文件互相踩）；如果是，那"并行"实际只在跨工作区成立，文案和交互要按这个前提设计。

### 3.2 运行中不能排队下一条消息（能力已有，入口被前端堵住）

- **现状**：后端每会话可排队 5 条（[scheduler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve/scheduler.rs#L32)），`POST /prompt` 立刻返回 202 和排队位置（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1548-L1562)）。但前端 `submit()` 在 `running` 时直接 return（[Composer.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Composer.jsx#L184)），状态接口里甚至写着"前端只用 running 判断是否禁用发送"（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1767-L1768)），排队位置从未展示。
- **成熟做法**：Claude Code / Cursor 都允许在 agent 跑的时候继续追加指令，排队并在界面上显示"已排队"。
- **补上要动**：Composer 放开 running 限制、把消息作为"排队卡"插入对话流；用已有的 `/status` 的 `position` 展示队列。若要"撤回某条排队消息"，目前只能整轮 interrupt（会连排队一起清空，[scheduler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve/scheduler.rs#L719-L729)），需要新增按项移除的方法。
- **标签**：确实缺（界面未接）。改动集中在前端 + 一个可选的后端方法。

### 3.3 历史会话是有损的：长回答和长工具输出会被永久截断

- **现状**：会话视图里 assistant 文本截到 12000 字符、思考截到 4000、工具入参截到 2000、工具输出截到 4000（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L4410-L4440)、[truncate_middle](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L4508)），审批入参截到 2000（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1087)）。刷新页面或重新打开会话后，超长回答和测试日志只显示"…（截断 N 字符）…"，界面上没有任何"查看全文"入口。
- **成熟做法**：历史完整保存、按需展开；Claude Code/Cline/OpenHands 都能翻完整的命令输出，长内容折叠而不是截断。
- **补上要动**：后端加"按消息/块取全文"的只读端点（或对视图接口做分页），前端在进程卡和回答底部加"展开全文"；同时保证 SSE 帧不要因为放大限制而爆掉（现在的截断大概率是为这个）。
- **标签**：确实缺（数据在 JSONL 里，界面拿不到）。典型"平时能用、真排查时抓瞎"。

### 3.4 计划与进度不是"活的"

- **现状**：任务面板只在挂载或手动点刷新时拉一次（[TasksPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/TasksPane.jsx#L11-L25)、[刷新按钮](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/TasksPane.jsx#L45)）；流式事件只有文本/思考/工具/起止/错误（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1293-L1319)），运行时事件枚举里同样没有 todo/plan 变更（[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L1213-L1251)）。计划模式只显示一个只读 chip，不能在界面里确认/修改计划。
- **成熟做法**：Claude Code 的 TodoWrite 清单常驻可见并逐项打勾；Devin 的计划带进度与阻塞点；Manus 有步骤进度面板；Cursor 的 to-do 随 agent 动作实时更新。
- **补上要动**：最省事的路径是在后端流式转发时识别 `TodoWrite`/计划类工具调用，解析入参补发 `todo_update` 事件（不必改运行时）；前端 TasksPane 订阅事件，并把当前计划卡内嵌到对话流；计划模式的"确认/驳回"需要后端补一个动作（现在只有 EnterPlanMode/ExitPlanMode 的被动识别）。
- **标签**：确实缺。**不确定**：长任务里 agent 实际写 TodoWrite 的频率有多高，如果很低，先做实时刷新收益有限。

### 3.5 审查闭环只做了一半：能"丢弃"，不能"回滚"，也不能"对着某行说"

- **现状**：变更面板能按 hunk 丢弃（[DiffPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/DiffPane.jsx#L32-L51)），但 agent 的修改是直接写进工作区的，没有"接受/拒绝"这一步；也没有 checkpoint，无法回到"上一轮之前"；不能选中 diff 里某一行发起提问或让 agent 改；会话级也没有分支/分叉。另外"丢弃"依赖 git 工作区，非 git 项目下这个面板基本失效（代码里没有显式提示）。
- **成熟做法**：Cursor 的 diff 逐块接受/拒绝；Cline 用影子 git 做 checkpoint，可一键恢复；Claude Code `/rewind` 回到任意一轮；OpenHands 可回放轨迹。
- **补上要动**：后端在每轮前后做快照（影子仓库或文件级备份）并提供 restore 端点；前端补"回到这一轮之前"按钮、diff 行级评论（评论拼成下一条 prompt）。"接受"在当前"先写盘"模型下语义不同，重点应放在回滚与行级追问，而不是为了对齐而硬做接受按钮。
- **标签**：半缺。改动面最大，但它决定你敢不敢放手让 agent 改代码。

### 3.6 工具调用展示仍是原始 JSON（同一仓库的 TUI 反而更好）

- **现状**：工具卡只显示工具名 + 原始入参字符串 + 原始输出文本，靠正则识别输出是不是 diff 来上色（[Conversation.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Conversation.jsx#L121-L145)）；分类摘要只有"已读取文件/已搜索代码/修改了文件"等粗粒度（[turns.js](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/turns.js#L1-L13)）；没有每次调用耗时、退出码、读取的行号范围。而本仓库的 TUI 已经按工具定制渲染：bash 显示命令、Read 显示路径、Edit 生成结构化补丁预览（[tool_display.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tool_display.rs#L374-L439)、[format_patch_preview](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tool_display.rs#L610-L635)），Web 没有复用这套逻辑。
- **成熟做法**：Claude Code 的命令/文件/搜索各有专属卡片；Cline 编辑卡片直接给 diff；Cursor 在对话里内联 diff。
- **补上要动**：前端解析入参 JSON 做分工具渲染（纯前端就能做，成本最低）；后端在 `tool_use/tool_result` 事件里补时间戳以计算耗时，或直接给出结构化的 `summary/path` 字段；Edit/Write 卡片复用现有补丁上色组件。
- **标签**：有但粗糙（信息全在，只是没加工）。

### 3.7 会话检索与管理：只能按标题过滤，且列表会静默少一截

- **现状**：侧栏过滤只匹配标题/会话 ID/工作区路径（[Sidebar.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/shell/Sidebar.jsx#L14-L19)），**不能搜消息正文**。`all=1` 聚合模式对每个工作区只取最近 50 条，`total` 也按截断后的数量算（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L829-L849)），所以第 51 条以后的历史会话在侧栏里根本不存在；前端请求 `limit=200`（[App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L91-L110)）会被服务端 clamp 到 100（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L826)）。没有置顶、归档、标签，也没有导出（CLI 有 `/export` 实现，见 [commands_handler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/commands_handler.rs#L746)，Web 无对应路由）。
- **成熟做法**：ChatGPT/Claude 全库会话搜索；Cline 任务历史可搜可删；OpenHands 会话列表带标题与状态；多数产品支持导出/分享当前会话。
- **补上要动**：修 `all=1` 的分页（按工作区各取全量元数据再合并排序，或改成游标）；加搜索端点（标题 + JSONL 正文扫描，规模大了再考虑索引）；会话元数据文件扩展 `pinned/archived`；加 Markdown 导出端点（后端有现成导出逻辑可借鉴）。
- **标签**：确实缺 + 一个明确缺陷（50 条截断）。**不确定**：会话量级（几百还是几万）决定要不要建索引，现在没有数据。另：CLI 的 `/search` 目前在桩命令列表里（[commands_handler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/commands_handler.rs#L1401)），不能当作"后端已实现、只差界面"。

### 3.8 没有消息级操作：不能重生成、编辑上一条、分叉、删除某轮

- **现状**：助手消息只有"复制"（[Conversation.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Conversation.jsx#L100-L118)），用户消息没有任何操作（[Conversation.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Conversation.jsx#L86-L98)）；后端没有"截断到某条消息"或"从此分叉"的接口（路由表见 [serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L615-L691)）。
- **成熟做法**：ChatGPT 编辑任意一条用户消息并重跑；Cline "从这条重试"；Cursor restore checkpoint 后重来。
- **补上要动**：后端加"把会话截断到第 N 条消息"（重写 JSONL）与"复制会话到新 ID"（分叉）；前端在消息头加"重试/编辑/从此分叉"菜单。要与 TUI 共用同一份 JSONL，写之前得确认没有其他端在跑该会话。
- **标签**：确实缺，改需求的频率很高。

### 3.9 审批交互粗糙：全屏挡视线、没有"本会话允许"、没有倒计时和 diff

- **现状**：审批是覆盖全屏的弹层（[App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L625-L640)、[styles.css](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/styles.css#L1022)），只有"允许/拒绝"两个按钮；没有"本会话内总是允许这个工具"；120 秒超时自动拒绝是写死的（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1066)），界面上只写了一句提示、没有倒计时；Edit 类工具只显示原始 JSON（截断 2k），看不到到底改哪几行。
- **成熟做法**：Cline/Cursor 把确认做成对话内联卡片，支持"总是允许这条命令/这个工具"，编辑操作先给 diff；Claude Code 的权限提示同样支持记住选择。
- **补上要动**：前端改成对话内联卡片 + 倒计时 + Edit/Write 的 diff 预览（用 `old_string/new_string` 本地算）；后端给 `WebPermissionPrompter` 增加按会话的允许清单并持久化（现在只有"完全访问"这种全局开关）。
- **标签**：有但粗糙。改动不大，体验提升明显。

### 3.10 内容呈现质量：Markdown 只支持一小半

- **现状**：自研渲染器只认 `-`/`*` 无序列表，**不认有序列表**（[markdown.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/markdown.jsx#L99-L104)），不支持嵌套列表、行内图片、任务列表、数学公式；代码块没有语法高亮，只有"复制"（[markdown.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/markdown.jsx#L79-L83)）。agent 很爱用 `1. 2. 3.` 列步骤，现在会被渲染成一整段文字。
- **成熟做法**：所有成熟产品都用标准 Markdown + 语法高亮；代码块有语言标签、行号、复制。
- **补上要动**：换 `markdown-it`（或 marked）+ 高亮库并离线打包。注意本项目"单文件内嵌、默认离线、无 CDN"的构建约束（[方案](file:///d:/claw-code-src/docs/2026-10-24-personal-tui-and-web-ui-plan.md#L20)、[方案](file:///d:/claw-code-src/docs/2026-10-24-personal-tui-and-web-ui-plan.md#L112)），需要评估产物体积；也可以只补有序/嵌套列表和图片这个最小集合。
- **标签**：有但粗糙，改动小、感知强。

### 3.11 文件面板只是"文本查看器"，编辑器是裸 textarea

- **现状**：文件列表平铺（上限 1500 条展示），没有目录树、没有 git 状态标记（[FilesPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/FilesPane.jsx#L130-L140)）；编辑用一个 `<textarea>`，无行号、无缩进、无语法高亮、无保存快捷键（[FilesPane.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/FilesPane.jsx#L107)）；超过 2MB 直接拒绝预览、文本超过 512KB 截断（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L484-L485)、[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L2066-L2075)）；没有"用本机编辑器打开"。
- **成熟做法**：OpenHands 的工作区有文件树和正经编辑器；Cursor 直接把改动交给 IDE；Cline 依赖 VS Code 的 diff 视图。
- **补上要动**：目录树可以纯前端用现有扁平路径拼；编辑器引入 CodeMirror 6（比 Monaco 轻）或先做"在 VS Code 打开"链接（`vscode://file/...`，成本极低但依赖本机装了 VS Code）。
- **标签**：有但粗糙。**不确定**：Web 端编辑代码的真实使用频率——如果大家还是回 IDE 改，就不该在这上面投入。

### 3.12 键盘与命令体系单薄

- **现状**：全局只有 `Ctrl/Cmd+K`（聚焦搜索）、`Ctrl/Cmd+B`（收侧栏）和 `Esc`（关设置）三个键位（[App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L265-L279)）；没有新建会话、切换会话、切换面板、取消本轮、命令面板。斜杠命令只有 6 个"打开某个面板"（[constants.js](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/shared/constants.js#L11-L18)）。CLI 侧确实有 `/export`、`/init`、`/status`、`/compact`、`/context` 等会话语义命令（[commands_handler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/commands_handler.rs#L728-L746)、[session_mgr.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/session_mgr.rs#L371)、[session_mgr.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/session_mgr.rs#L707)），Web 一个都没接；`/rewind`、`/search` 在 CLI 侧本身还是桩（[commands_handler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/commands_handler.rs#L1361)、[commands_handler.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/commands_handler.rs#L1401)），不计入"可复用能力"。
- **成熟做法**：命令面板（`Cmd/Ctrl+Shift+P` 那种）+ 全套快捷键；斜杠命令是真实动作而不是导航别名。
- **补上要动**：前端做快捷键注册表和命令面板；斜杠命令需要后端补对应路由（导出、初始化项目说明、状态、目标管理），实现可以借鉴 CLI 已有逻辑。注意 CLI 命令处理是面向 TUI 的，直接复用要先抽一层。
- **标签**：确实缺。高频用户感知很强，但优先级低于 3.1–3.5。

### 3.13 没有"产物"概念与预览

- **现状**：聊天里不内联图片（Markdown 不支持图片），生成的网页/报告没有预览入口（只有文件面板的 PDF iframe 和图片预览），没有"本轮产出了哪些文件"的清单，下载要自己拼 `/api/workspace/raw` 的 URL。
- **成熟做法**：Manus 有产物面板（文件/网页/幻灯片）；Claude artifacts 在侧栏实时渲染 HTML/SVG；Devin 直接给 PR 和预览链接；ChatGPT canvas 同理。
- **补上要动**：消息里加 attachment 块（图片缩略图、文件卡）；后端记录每轮写过的文件；HTML 用 `iframe sandbox` 预览；加下载端点（可复用 raw，但要补 `Content-Disposition`）。
- **标签**：确实缺。**不确定**：这个项目的日常任务是写代码还是也做文档/网页，如果纯写代码，优先级可以往后放。

### 3.14 上下文占用与压缩过程不可见

- **现状**：流式期间不推 token 用量——`StatusEvent::Usage` 在转发时直接 `return` 丢掉（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1321)）；统计只在轮末从快照算（[Composer.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Composer.jsx#L703-L708)），没有"上下文窗口用了多少"的刻度，也没有压缩发生的提示——尽管运行时已有压缩状态枚举（[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L1566-L1574)）。
- **成熟做法**：Cline 有上下文窗口占用条和成本估算；Claude Code 会明确提示"正在压缩上下文"；Cursor 也展示用量。
- **补上要动**：转发 `Usage` 与压缩状态事件（运行时已有，只是没接）；前端在输入区上方加占用条；需要各模型上下文上限的配置来源。
- **标签**：有但粗糙，长会话下会变成"确实缺"。

### 3.15 终端与会话工作区不一致（明确缺陷）

- **现状**：终端 WebSocket 固定使用 serve 启动时的工作区（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L1784)），前端终端只在第一次打开时挂载并跨会话复用（[Panel.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/Panel.jsx#L11-L13)、[Panel.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/panel/Panel.jsx#L37)）。所以在界面里切到另一个工作区的会话后，终端仍然停在启动目录——你在错误的仓库里执行命令，且界面没有任何提示。只有单终端，也没有与 agent 的 bash 输出打通。
- **成熟做法**：OpenHands 的终端跟随当前工作区；Cursor 每个工作区一个终端；多数产品支持多终端标签。
- **补上要动**：终端 URL 带 `workspace` 参数并复用 `resolve_workspace`；工作区变化时重建 PTY；多终端需要后端把 PTY 会话化（现在是一次性连接）。
- **标签**：缺陷（不安全），建议提前到 P0 末尾——改动很小。

### 3.16 子代理过程不可见

- **现状**：运行时有多智能体/子代理能力（`SubagentStatus` 的 created/running/completed/failed/cancelled，[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L7836-L7846)），但流式事件枚举里没有子代理状态（[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L1213-L1251)），Web 端只会在调用处显示一个工具卡，然后静默等待。
- **成熟做法**：Claude Code 把子代理显示成嵌套的任务卡，能看到它读了哪些文件、什么时候结束。
- **补上要动**：运行时补子代理状态事件 → serve 转发 → 前端嵌套渲染。
- **标签**：确实缺。**不确定**：Web 这条路径上实际触发子代理的频次（TUI 侧有，Web 侧未验证）。

### 3.17 设置页过于单薄

- **现状**：设置只有主题和默认权限（[SettingsPage.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/settings/SettingsPage.jsx#L1-L43)）；服务端可持久化的字段也只有主题、权限、面板布局、模型（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L454-L463)）。模型和权限分散在输入区/TopBar/设置三处，没有通知开关、快捷键、默认工作区、数据清理。
- **成熟做法**：成熟产品的设置页覆盖通知、外观、快捷键、模型/密钥、数据与隐私。
- **补上要动**：扩展 `WebSettingsBody` 与设置页；把通知、快捷键、默认工作区收进来；清理会话/缓存的入口。
- **标签**：有但粗糙，低风险。

### 3.18 窄屏与断线恢复

- **现状**：窗口宽度小于 1100px 时整个右侧面板直接 `display: none`，没有替代入口（[styles.css](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/styles.css#L1047-L1052)）；小于 840px 侧栏变成绝对定位抽屉。服务重启导致 token 更换时，只能按提示"用新打开的页面继续"（[useSessionEvents.js](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/useSessionEvents.js#L76-L79)），历史消息不会丢，但当前操作会断。
- **成熟做法**：窄屏下面板改成可切换的浮层/抽屉；服务重启后自动恢复会话连接。
- **补上要动**：CSS 与面板容器改浮层；断线时用 `/api/meta` 探测新 token（热升级路径已有类似轮询，[App.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/App.jsx#L171-L190)）。
- **标签**：有但粗糙。如果只在桌面全屏用，可以不管。

---

## 4. 不算缺口的差异（属于取舍）

以下是和成熟产品的差异，但不建议计入缺口：

- **多用户、鉴权体系、公网暴露**：方案里已明确不做（[明确不做](file:///d:/claw-code-src/docs/2026-10-24-personal-tui-and-web-ui-plan.md#L185-L190)）。当前一次性 token 放 URL、只绑 127.0.0.1 的做法在这个前提下是合理的。
- **云端执行/远程沙箱**：输入区的"远程 / 云端"是显式禁用的（[Composer.jsx](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/web/src/chat/Composer.jsx#L666)），属于未启用而非遗漏。
- **移动端适配**：没有证据表明有移动端使用需求；3.18 只按"窄窗口"记，不当成"缺移动端"。
- **图片协议渲染**：方案里明确不做（没有图片工具就先不实现）。现在有上传 + 附件预览，够用。
- **插件/技能市场、语音讨论、多工作区、每会话草稿、逐 hunk 丢弃**：这些是相对优势，不应被"对标清单"稀释。
- 方案本身也写了"不做功能对齐清单式的堆砌"（[方案](file:///d:/claw-code-src/docs/2026-10-24-personal-tui-and-web-ui-plan.md#L190)），所以本清单只保留"单人日常真的会按"的项。

---

## 5. 如果只做三件事

1. **3.1 + 3.2 + 3.15**：让"并行"真正可用（侧栏状态 + 完成通知 + 允许排队 + 终端跟随工作区）。后端基本都有，主要是界面接线，投入产出比最高。
2. **3.4**：让计划/待办实时可见。长任务里"它到底在干什么"比任何花哨功能都重要，而且可以先用"识别 TodoWrite 工具调用补发事件"的低成本路径。
3. **3.3 + 3.6**：让历史和工具输出可读（按需全文 + 分工具渲染 + 耗时）。这是排查 agent 出错时最直接的抓手，TUI 已有可借鉴的实现。

下一批建议做 **3.5（审查闭环/回滚）**——它改动最大，但决定你敢不敢把重构类任务交给 agent。3.7–3.12 属于"用久了会烦"的层级，可在前两批之后按实际痛点挑。

---

## 6. 不能确认的地方

- 各成熟产品的最新形态（2026 年）未逐版本核对，矩阵里描述的是其公开版本的通用做法；个别交互细节可能有出入。
- 每工作区并发 1 是否为刻意的安全设计，代码里没有注释说明；如果不是刻意的，3.1 的"并行"价值会比现在大很多。
- 前端构建把 `web/dist/index.html` 单文件内嵌进二进制（[serve.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/serve.rs#L45)）。引入 Markdown 渲染器/编辑器库后的产物体积与"无 CDN、离线可用"约束的冲突程度，未实测。
- 会话正文搜索的可行性取决于会话规模与单个 JSONL 大小，本次没有统计真实数据量。
- Web 路径上子代理的实际触发频率未验证（TUI 侧确定有）。
- 第 7 节已实际启动服务做交互核验（独立端口、临时工作区），覆盖 15 个重点项；**子代理（3.16）、断线重连、热升级换 token 仍未实测**，其余行为按代码路径推演的部分已在第 7 节标注。
- CLI 侧 `/rewind`、`/search` 仍是桩命令，`/export`、`/init`、`/status`、`/compact`、`/context` 有实现；其余命令的完成度未逐一核对，因此"复用 CLI 逻辑"的工作量不确定。

---

## 7. 真实界面核验（2026-11-15 追加）

### 7.0 环境与做法

- 用当前源码自行编译，在**独立端口 4399**、**临时工作区** `%TEMP%\claw-ui-verify`、`%TEMP%\claw-ui-verify2` 上启动；全程未重启、未占用桌面上正在运行的旧实例端口。
- 数据是造出来的，不看代码下结论：
  - 60 个会话文件（验证 50 条上限）；
  - 15030 字助手消息、9000 字工具输出（验证截断）；
  - 向 JSONL 追加第 4 条 `TodoWrite`（验证计划面板实时性）；
  - git 工作区真实改动（验证 diff 展开、丢弃）；
  - Markdown 有序/无序列表、代码块、表格（验证渲染）。
- 驱动方式：无头 Chromium 实际点击，截图落在 `%TEMP%\claw-ui-shots`（临时目录，未入库）。
- 判定沿用三分类：**真没有**（界面找不到入口，也无间接路径）/ **有，但藏得深**（能力在，入口隐蔽或需特定条件）/ **有，且好用**（能直接用到）。

### 7.1 判定总表（用户指定的 15 项）

| # | 核验项 | 判定 | 实际触达路径 | 观察到的现象 |
|---|---|---|---|---|
| 1 | 并行任务标记与完成通知 | **真没有** | 无路径 | 一个会话在跑时侧栏会话行与平时完全一样（只有标题 + 相对时间，无状态点/未读/转圈）；换到别的会话后看不到原会话进度；跑完没有 toast、标题栏不变、无 `Notification` |
| 2 | 运行中追加消息 | **真没有** | 无路径 | 运行中发送按钮变"停止"；在输入框按 Enter 不产生任何 `/prompt` 请求，文本原样留在输入框（前端 `running` 时 `submit()` 直接 return） |
| 3 | 长内容截断后看全文 | **真没有** | 无路径 | 15030 字助手消息实际只渲染 12046 字，中间出现"…（截断 3030 字符）…"，首尾标记都在；消息上唯一的按钮是"复制"，没有"展开全文" |
| 4 | 任务面板是否实时 | **真没有** | 右侧面板 → 任务 | 面板挂载时拉一次数据；追加第 4 条 TodoWrite 后等 6 秒不出现，点"刷新"才出现 |
| 5 | 计划模式能否确认修改 | **真没有** | 右侧面板 → 任务 → "计划模式"标签 | 该标签是 `<span class="chip">`，鼠标不是手型、点了没有任何反应；没有"批准/修改计划"按钮 |
| 6 | diff 按行提问与回滚 | **有，且好用**（仅 hunk 级回滚） | 右侧面板 → 变更 → 点 `src/a.py` → 每个 hunk 右侧"丢弃此块" | 点"丢弃此块"后 `POST /api/workspace/diff/discard` 返回 ok，文件内容恢复、面板变成"0 个已修改"；**但没有行级操作**：点击任意 diff 行无菜单、无选中态、无"问这段"入口，也不能只回滚某一行 |
| 7 | 工具卡渲染形态 | **有，但藏得深** | 对话流 → "✦ 修改了文件"（过程摘要条）→ 展开 → `<details>` 卡片 | 默认折叠成一行摘要（"已读取文件/搜索了代码/修改了文件"）；展开后是工具名 + `<pre>` 原始 JSON 入参（`{"file_path": "src/app.py", ...}`）+ 纯文本输出，无分工具渲染、无耗时、无退出码 |
| 8 | 会话列表 50 条上限是否静默截断 | **真没有**（51 条以后无法触达） | 侧栏 → 工作区计数 | 临时工作区实际 60 个会话文件，项目徽标显示 **50**，`.session-row` 恰好 50 条；侧栏底部"加载更多（100/179）"点一次后按钮消失、行数仍是 50（只多加载别的工作区）——第 51–60 条永远看不到，且没有任何提示 |
| 9 | Markdown 有序列表 | **真没有** | 对话流里的回答 | 实测 `ol=0 / ul=1 / pre=1 / table=1 / 高亮=0`："1. 第一步 2. 第二步"被渲染成普通段落，代码块无语法高亮 |
| 10 | 文件面板 | **有，且好用** | 右侧面板 → 文件 → 点文件 → "编辑" → "保存" | 平铺列表（无目录树）→ 预览带行号（1/2/3/4）→ 编辑为裸 textarea（无行号/无高亮）→ 保存后**文件真实写盘**（重新读取内容已变） |
| 11 | 审批交互 | **有，且好用** | 输入区权限选"工作区可写" → 让 agent 调 Bash（需提权）→ 全屏遮罩 | 遮罩显示工具名 `bash`、所需模式"完全访问"、原因、截断后的入参；按钮只有"拒绝/允许"，允许后本轮继续执行；没有"本会话总是允许"、没有倒计时、没有 diff 预览 |
| 12 | 快捷键与斜杠命令清单 | **真没有**（缺的部分） | 键盘 / 输入框打 `/` | 全局只有 `Ctrl+K`（聚焦搜索，实测光标进入搜索框）、`Ctrl+B`（实测 `aside.collapsed` 切换）、`Esc`；无新建/切换会话/命令面板/取消。输入 `/` 只出 6 条面板导航（文件/变更/任务/终端/附件/设置），无 `/init`、`/export`、`/compact` 等会话语义命令 |
| 13 | 上下文占用显示 | **真没有** | 输入区下方统计 | 只有轮末"4 轮 17 步 · 15 tok/s · 17.8k tokens · 缓存 xx%"，是累计用量；没有上下文窗口占用条/百分比，流式期间无用量显示 |
| 14 | 终端 cwd 跟随工作区 | **真没有** | 右侧面板 → 终端 → 输入 `pwd` | 终端 cwd = serve 启动目录 `...\Temp\claw-ui-verify`；切到 `claw-code-src` 工作区的会话后再 `pwd`，仍然停在 `claw-ui-verify`，界面无任何提示 |
| 15 | 窄屏布局 | **真没有**（面板不可用；侧栏可收起） | 缩到 1024 / 800 / 700 / 500px | <1100px 时 `.panel` 被 `display:none`，但顶栏按钮仍显示"隐藏面板"（状态与实际不符），点"显示面板"后计算样式仍是 `none`——面板彻底不可用；<840px 侧栏变绝对定位浮层（320px），可用 `Ctrl+B` 收起，但需要手动 |

### 7.2 原判定错误（明确修正）

| 原编号 | 原判定 | 核验后 | 说明 |
|---|---|---|---|
| 3.5 | "审查闭环只做了一半：能'丢弃'，不能'回滚'" | **原判定错误** | "丢弃此块"就是逐 hunk 回滚：实测点击后文件恢复到 git 版本、面板改回"0 个已修改"，后端 `diff/discard` 带 fingerprint 校验有效。真正缺的是**行级提问/行级回滚/checkpoint**，不是回滚本身。 |
| 3.11 | "文件面板只是'文本查看器'" | **原判定错误** | Web 端可以编辑并写盘：文件 → 点文件 → 编辑 → 改内容 → 保存，实测磁盘内容已更新。问题在编辑器质量（裸 textarea、无行号、无高亮、无保存快捷键），不是"只能看"。 |
| 3.14 | "有但粗糙" | **判定偏轻** | 界面完全没有上下文窗口占用与压缩提示，只有轮末累计 token；按本次标准应记为"确实缺"（占用条），累计统计只能算附带信息。 |

### 7.3 补充细节（核验中观察到的行为，供修复时对照）

- **截断是"掐中间"不是"砍尾巴"**：15030 字 → 12k 保留首尾，中间替换成"…（截断 N 字符）…"。所以上下文开头和结论都在，丢的是中段。工具输出同理（9000 字 → 4000）。
- **50 条上限是"每工作区"的硬上限**：`all=1` 聚合时每个工作区取最近 50；实测临时工作区 60 个文件、接口与界面都只有 50。项目徽标数字 = `min(会话数, 50)`，所以徽标本身也会骗人。
- **搜索只匹配标题/ID/路径**：搜正文关键词（`REALTIME-PROBE-4`）→ 0 条并提示"没有匹配的会话"；搜标题关键词 → 命中。与 3.7 一致。
- **任务面板里有两套东西**：上半部"当前待办（可编辑 · 下一条消息会带给 agent）"可增删改，会拼进下一条 prompt；下半部"历史 TodoWrite（只读）"是 agent 写过的待办快照。只有前者能操作，后者不会自动刷新。
- **计划模式只在 `plan_mode=true` 时出现一个不可点的标签**；`EnterPlanMode` 在对话流里只显示成一张工具卡（"更新了计划"），没有计划正文卡。
- **工具卡的单条形态**是 `<details class="process-tool">`，摘要写着工具名（如 `Edit`），内文是 `<pre class="tool-input">` 原始 JSON；没有结果的旧调用会一直显示为 `running`。
- **审批只在"越权"时出现**：只读模式下直接改文件是被硬拒绝（不弹审批），工作区可写模式下调 Bash 这类需要"完全访问"的工具才会弹。
- **窄屏按钮状态会撒谎**：1024px 下按钮文案还是"隐藏面板"，说明布局状态与 CSS 断点没有联动。

### 7.4 核验顺带发现（不在原清单）

- 变更面板丢弃成功后，列表已变"0 个已修改"，但下方仍残留上一份 patch 文本，需要手动刷新才消失。
- 侧栏项目列表混入大量 0 会话的历史工作区（`38225`、`s2a-serve-a`、`p0-ws`… 共 14 个项目），项目一多会挤占侧栏首屏。
- 会话视图的"84 条"类徽标是消息条数，与项目徽标（会话条数）语义不同，容易混淆。

### 7.5 统计

- **真没有：11 项**（并行标记与通知、运行中追加、长内容全文、任务面板实时、计划模式确认修改、50 条上限外的会话、Markdown 有序列表、快捷键/语义斜杠命令、上下文占用、终端 cwd 跟随、窄屏面板）。
- **有，但藏得深：1 项**（工具卡渲染——能力在，但默认折叠在过程摘要条里，展开才是原始 JSON）。
- **有，且好用：3 项**（逐 hunk 回滚、文件预览/编辑/保存、阻塞式审批）。
- **原判定错误：2 项**（3.5 回滚、3.11 文件编辑）；另有 1 项判定偏轻（3.14 上下文占用）。

> 核对时间：2026-11-15；服务已在核验结束后停止并释放 4399 端口，临时工作区与截图目录为 `%TEMP%` 下的验证残留。
