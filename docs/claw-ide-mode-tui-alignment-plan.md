# Claw IDE 模式全面对齐 TUI 功能规划

> 版本：v1.0
> 日期：2026-08-14
> 范围：将 `vscode-extension`（Trae IDE 集成）的功能对齐 `rusty-claude-cli` TUI 模式，并发挥 IDE 环境的集成化优势
> 状态：规划（Plan Mode，待用户批准后执行）

---

## 一、摘要

当前 IDE 集成（`vscode-extension`）通过 ACP 协议桥接 `claw-plus-headless` 子进程，已经打通了「编辑器 ↔ agent」的通信骨架：`fs/read_text_file`（读未保存 buffer）、`fs/write_text_file`（走 undo 栈）、`session/request_permission`（危险操作弹窗）、LaneEvent → SessionNotification 桥接、Session Bus 互通均已实现。

但 IDE 模式仍处于 MVP 阶段，存在三大系统性问题：

1. **功能简陋**：`prompt` 是 turn 完成后一次性推送，无流式输出；会话恢复/分叉/模式切换/模型切换后端均未实现；前端渲染层是纯文本 `appendMsg`，无 markdown、无 tool card、无 thinking 折叠、无状态栏 token/cost。
2. **界面粗糙**：聊天面板 HTML 仅是一个 `<textarea>` + `<div>` 追加纯文本，与 TUI 的 ToolCard 折叠、Timeline 视图、Thinking 卡片、Sticky 头部、历史重放等丰富展示相比差距巨大。
3. **初次配置不友好**：binary 靠 PATH 解析（GUI 进程 PATH 与 PowerShell 不一致导致 ENOENT）、`--version` 探测对 headless 无效、多 provider 无引导、无诊断。

本规划目标是：**分三个阶段（P0/P1/P2）系统性补齐**，最终让 IDE 模式具备 TUI 的完整功能集，同时发挥「可撤销写文件、读未保存内容、可视化权限审批、多面板并行、编辑器上下文感知、LSP 诊断集成、原生 diff 视图」等 TUI 无法提供的 IDE 集成化优势。

---

## 二、现状分析（功能对比）

> 探索结论基于真实源码核对，关键文件均已通读（见文末「参考文件清单」）。

### 2.1 后端（Rust）能力对比

| 功能 | TUI 实现 | IDE(ACP) 现状 | 差距定位 |
|---|---|---|---|
| 流式输出 | ✅ `StatusEvent::TextDelta` 逐 delta 渲染 | ❌ `prompt` 在 turn 完成后一次性 `AgentMessageChunk` | [agent.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs#L369-L378) |
| cancel 中断 | ✅ `HookAbortSignal` | ✅ 已实现真实中断 | 无差距 |
| 工具调用展示 | ✅ ToolCard（start/result 分离） | ⚠️ 后端推 `ToolCall`，前端未渲染 | 前端差距 |
| Thinking 展示 | ✅ 折叠卡片 | ❌ 未推送 | 前后端均缺 |
| 会话恢复 load | ✅ `resume_session` | ❌ `method_not_found` | [agent.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs#L285-L291) |
| 会话切换 switch | ✅ `handle_session_command` | ❌ 未实现 | 后端 + 前端 |
| 会话分叉 fork | ✅ `runtime.fork_session` | ❌ 未实现 | 后端 |
| plan/act 模式 | ✅ `/permissions`、mode | ❌ `method_not_found` | [agent.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs#L293-L298) |
| 模型切换 | ✅ `/model` | ❌ 未实现 | 后端 + 前端 |
| 历史搜索 | ✅ hybrid（FTS5+向量） | ✅ 经 `session_search` 工具可用 | 前端 UI 缺失 |
| 上下文压缩 | ✅ `/compact` | ✅ 经工具可用 | 前端 UI 缺失 |
| 权限审批 | ⚠️ TUI 静默拒绝 | ✅ IDE modal 弹窗（更优） | IDE 已占优 |
| LaneEvent 桥接 | ✅ | ✅ 已实现（21 种事件映射） | 无差距 |
| Session Bus | ✅ | ✅ 已实现 | 前端未渲染 |
| **多 session 并发** | ⚠️ 单 runtime | ❌ **单 session 模型**（`api_client` 只 take 一次） | 核心架构缺口 |

**关键发现**：`ClawAgent` 是单 session 模型——`new_session` 用 `api_client.borrow_mut().take()` 一次性消费，第二次 `new_session` 返回 `"api_client already consumed"`。这导致 IDE 前端的「多 panel 并行」在 `chat-panel.ts` 里是假象，实际后端只支持一个会话。**这是「发挥 IDE 多面板优势」的结构性障碍。**

### 2.2 前端（TypeScript）能力对比

| 功能 | TUI | IDE 现状 | 差距 |
|---|---|---|---|
| Markdown 渲染 | ✅（标题/代码块/加粗/表格换行保护） | ❌ 纯文本 | 需重写渲染层 |
| ToolCard 折叠 | ✅（>5 行折叠、输入/结果分离、diff 高亮） | ❌ | 需重写渲染层 |
| Thinking 折叠 | ✅（`▶ Thinking (N chars)`） | ❌ | 前后端 |
| Timeline 视图 | ✅（`🔧 bash → ✓`） | ❌ | 前端 |
| 状态栏 token/cost | ✅（`pricing_for_model` + 本地化） | ❌ 仅 running/error 五态 | [status-bar.ts](file:///d:/claw-code-src/vscode-extension/src/status-bar.ts) |
| 侧边栏（会话/用量/工具/技能） | ✅ | ❌ | 前端 |
| Slash 菜单（fuzzy 搜索） | ✅ | ❌ | 前端 |
| 历史回看/重放 | ✅（session JSONL 重放） | ❌ | 前后端 |
| 模型/effort 切换 UI | ✅ | ❌ | 前端 + 后端 |
| 多会话面板 | N/A | ✅ 已有多 panel 结构（但后端不支持） | 后端限制 |

### 2.3 核心差距总结（按影响排序）

1. **流式输出缺失**（影响最大）：TUI 的即时反馈是核心体验，IDE 目前要等整个 turn 结束才看到结果。
2. **单 session 架构限制**：无法发挥 IDE 多面板并行、会话切换/恢复的核心优势。
3. **会话管理后端缺失**：load/fork/set_mode/set_model 全部 `method_not_found`。
4. **前端渲染层缺失**：markdown、tool card、thinking、timeline、状态栏 token/cost 全部没有。
5. **初次配置不友好**：ENOENT、`--version` 探测失效、无 provider 引导。

---

## 三、关键架构决策

以下决策基于现状代码与项目记忆（`project_memory.md`）中的约束推导，作为本次规划的不可变前提：

### D1：继续使用 Webview 自绘，不迁移到原生 Chat API
- **理由**：VS Code 原生 `chatParticipants` API 无法实现 ToolCard 折叠、Timeline 视图、Thinking 折叠卡片、Sticky 头部等 TUI 核心展示。复刻 TUI 丰富展示的唯一路径是 webview 自绘。
- **影响**：前端渲染层需完全重写为结构化条目渲染（对齐 TUI 的 `OutputEntry` 设计）。

### D2：保持 ACP 0.10.4 默认协议，不强行升级 1.5
- **理由**：项目记忆与 `ide-integration-detail.md` 明确 1.5 升级是 PoC 风险项，且 `Cargo.toml` 默认 feature 是 `acp-0_10`。本次规划不引入协议升级风险。
- **影响**：流式输出在 0.10.4 的 `SessionUpdate::AgentMessageChunk` 语义下实现（逐 delta 推送多个 chunk），会话管理用原生 ACP 方法 + 已有 `ExtNotification` 扩展通道。

### D3：后端引入「流式事件回调」机制，不修改 `ApiClient` 契约
- **理由**：`run_turn_async` 内部已有 `AssistantEvent` 枚举（`TextDelta`/`Thinking`/`ToolUse`/`Usage`），只需新增回调钩子把事件实时转发，不动 `ApiClient::stream_async` 的批量返回契约。
- **影响**：`ConversationRuntime` 新增 `stream_event_callback` 字段，`ClawAgent` 注入回调（闭包捕获可 `Clone` 的 `AcpGatewaySender`，规避 `&mut self` 借用冲突）。

### D4：后端从单 session 重构为多 session 架构
- **理由**：IDE 多面板并行 + 会话切换/恢复是 IDE 核心优势，单 session 模型必须打破。
- **影响**：`ClawAgent` 的 `runtime: RefCell<Option<...>>` 改为 `RefCell<HashMap<SessionId, ConversationRuntime>>`；`api_client`/`tool_executor` 需改为每个 session 独立实例（或支持从配置重建）。这是 P1 阶段最大的架构改造，风险最高。

### D5：状态栏 token/cost 数据源复用 `runtime::pricing_for_model`
- **理由**：项目记忆明确「StatusEmitter 是 TUI cumulative usage 唯一真相源」，token/cost 计算必须用 `runtime::pricing_for_model`，IDE 状态栏要与 TUI 一致。
- **影响**：后端需在 `prompt` 完成时推送 `Usage` 数据（`AssistantEvent::Usage` 已含 `TokenUsage`），前端状态栏据此 + 模型名计算成本。

---

## 四、优先级排序

| 阶段 | 目标 | 核心交付 | 验收门槛 |
|---|---|---|---|
| **P0** | 核心体验对齐（单 session 可用） | 流式输出 + 前端渲染引擎 + 状态栏 | 流式逐字显示、markdown 渲染、tool card 折叠、状态栏显示 token/cost |
| **P1** | 会话/模型管理对齐（多 session） | 多 session 架构 + load/fork/set_mode/set_model + 会话侧栏 | 可并行多面板、恢复历史会话、切换模型、plan/act 模式 |
| **P2** | IDE 差异化优势 + 细节打磨 | 上下文感知 + LSP 诊断 + diff 视图 + slash 菜单 + 配置向导增强 | 读当前文件自动注入、诊断反馈、diff 展示、配置零门槛 |

---

## 五、实施步骤

### P0：核心体验对齐

#### 5.1 后端流式输出（`rust/crates/runtime/src/conversation.rs` + `claw-shell/src/agent.rs`）

**改什么**：
- `ConversationRuntime` 新增字段 `stream_event_callback: Option<Box<dyn Fn(AssistantEvent) + Send>>`，新增 `with_stream_event_callback` setter（对齐现有 `with_hook_progress_reporter`/`with_tool_result_callback` 模式）。
- 在 `run_turn_async` 内部遍历 `AssistantEvent` 的位置（约 `conversation.rs:6908` 附近的 `AssistantEvent::TextDelta(delta) => text.push_str(&delta)` 分支），对 `TextDelta`/`Thinking`/`ToolUse`/`Usage` 调用回调。
- `ClawAgent::prompt` 在 `new_session` 构造 runtime 时注入回调：闭包捕获 `self.client_gateway.clone()`（`AcpGatewaySender` 是 mpsc sender，`Clone + Send`），把事件实时 `forward_fire_and_forget` 为 `SessionNotification`。

**为什么**：打破「turn 完成后一次性推送」的瓶颈，实现逐 delta 流式。

**映射关系**：
- `AssistantEvent::TextDelta(delta)` → `SessionUpdate::AgentMessageChunk(ContentChunk::new(TextContent::new(delta)))`
- `AssistantEvent::Thinking` → `SessionUpdate::AgentMessageChunk`（带 `[thinking]` 前缀，前端渲染为折叠卡片）
- `AssistantEvent::ToolUse` → `SessionUpdate::ToolCall(Pending)`
- `AssistantEvent::Usage` → 缓存在 agent 内，turn 结束随最终 chunk 推送（供状态栏 token/cost）

**关键约束**：回调必须是 `Send + 'static`；`AcpGatewaySender::forward_fire_and_forget` 不阻塞，符合 fire-and-forget 语义。

#### 5.2 前端渲染引擎重写（`vscode-extension/src/chat-panel.ts`）

**改什么**：将 `getChatHtml()` 里的纯文本 `appendMsg` 替换为结构化渲染器：
- 引入 `markdown-it`（或轻量 markdown 渲染库）做 markdown → HTML。
- 定义前端条目模型（对齐 TUI `OutputEntry`）：`Text` / `Thinking`（可折叠）/ `ToolCard`（start/result 分离，>5 行折叠）/ `Timeline` / `Error`。
- `routeSessionUpdate` 根据 `SessionUpdate` 类型分发：`agent_message_chunk` 累积为当前流式文本条目（delta 追加），`tool_call` 创建/更新 ToolCard，`tool_call_update` 更新状态。
- 新增 `session/peer_message`（ExtNotification）处理，展示 Session Bus 对等消息。

**为什么**：前端现在是最大短板，纯文本无法承载 TUI 的丰富展示。

**新增依赖**：`markdown-it` + `@types/markdown-it`（devDependencies）。

#### 5.3 状态栏增强（`vscode-extension/src/status-bar.ts` + `types.ts`）

**改什么**：
- `ClawStatus` 扩展字段：`model`、`turnCount`、`tokensIn/Out`、`costUsd`、`cwd`、`streaming`。
- 后端 `prompt` 完成时推送 `Usage` + 模型名（经 `session/update` 或专用 ExtNotification），前端 `StatusBarManager` 更新。
- `types.ts` 新增 `Usage` 相关类型。

**为什么**：对齐 TUI 状态栏的 token/cost/模型/计时器，用 `runtime::pricing_for_model` 保证一致性。

---

### P1：会话/模型管理对齐

#### 5.4 后端多 session 架构重构（`rust/crates/claw-shell/src/agent.rs`）

**改什么**：
- `ClawAgent.runtime: RefCell<Option<...>>` → `RefCell<HashMap<acp::SessionId, ConversationRuntime>>`。
- `api_client`/`tool_executor` 从「单份 take」改为「按需从配置/工厂重建」（需 `ClawAgentConfig` 持有可重建 api_client 的工厂，或改为每个 session clone 必要上下文）。
- `new_session` 不再 take 单份，而是为新 session 创建独立 runtime 并插入 map。
- `prompt`/`cancel` 按 `session_id` 从 map 取对应 runtime。

**为什么**：这是发挥 IDE 多面板并行优势 + 支持会话切换/恢复的前提。

**风险**：`StaticToolExecutor` 内含 `Box<dyn FnMut>` 非 Send，需保持「在 LocalSet 内构建」约束（现有 `build()` 已保证）。api_client 工厂必须是 `Send`（`ClawAgentBuilder` 已要求 `C: ApiClient + Send`）。

#### 5.5 会话管理后端补齐（`agent.rs`）

**改什么**：
- `load_session`：从 `.claw/sessions/{id}/session-*.jsonl` 加载历史 session（复用 `session_mgr.rs::resume_session` 逻辑），重建 runtime 并返回。
- `set_session_mode`：实现 plan/act 切换（映射到现有 permission/mode 逻辑）。
- `fork_session` / `set_session_model`：复用 `runtime.fork_session` 与 `set_model`。

**为什么**：对齐 TUI 的会话恢复/分叉/模式/模型切换。

#### 5.6 前端会话侧栏（`chat-panel.ts` 或新增 `session-list.ts`）

**改什么**：
- 新增会话列表视图（复用 VS Code `TreeDataProvider` 或 webview 内嵌列表）。
- 展示：会话 ID、cwd、消息数、最后活动时间、当前状态。
- 操作：新建/切换/恢复/删除/分叉。

**为什么**：对齐 TUI 侧边栏会话段，同时是 IDE 多面板的入口。

#### 5.7 前端模型/effort/权限 UI（`chat-panel.ts` + `extension.ts`）

**改什么**：
- 在聊天面板顶部或命令面板增加：模型选择、reasoning effort、权限模式切换。
- 对应调用后端 `set_session_model` / `set_session_mode` / 权限扩展。

**为什么**：对齐 TUI 的 `/model`、`/effort`、`/permissions` 命令。

---

### P2：IDE 差异化优势 + 细节打磨

#### 5.8 编辑器上下文感知（`extension.ts` + `handlers.ts`）

**改什么**：
- 监听 `vscode.window.onDidChangeActiveTextEditor` / `onDidChangeTextEditorSelection`，经 ACP `session/update` 或 ExtNotification 推送当前文件路径 + 选区给 agent。
- `ClawAgent` 侧接收后注入 `context_assembler`（对齐 `ide-integration-detail.md` 的 P1 设计）。

**为什么**：TUI 对「用户在看什么」一无所知，这是 IDE 的独家优势。

#### 5.9 LSP 诊断集成（`extension.ts`）

**改什么**：
- 监听 `vscode.languages.onDidChangeDiagnostics`，把当前文件的 diagnostics 摘要推送给 agent 作为上下文。
- agent 在修复代码时优先参考诊断结果。

**为什么**：IDE 独有优势，TUI 无法提供。

#### 5.10 Diff 视图（`handlers.ts` + `chat-panel.ts`）

**改什么**：
- `fs/write_text_file` 响应中附带 diff 信息（或用 `WorkspaceEdit` 后 diff），聊天面板展示红删绿增的 diff 视图（可用 VS Code 原生 diff editor）。

**为什么**：文件修改以 diff 呈现而非整体覆盖，发挥 IDE 原生能力。

#### 5.11 Slash 菜单（`chat-panel.ts`）

**改什么**：
- 前端实现 `/` 触发命令菜单（fuzzy 搜索 name/alias/summary，大小写不敏感），对齐 TUI `slash_menu.rs`。

#### 5.12 配置向导增强（`setup-wizard.ts`）

**改什么**：
- binary 探测改为文件选择器（`showOpenDialog`）而非 PATH 解析，根治 ENOENT。
- 多 provider 引导（DeepSeek/Anthropic/OpenAI/Qwen），对应 API key 分别存 SecretStorage。
- 集成 `doctor` 诊断（`spawn claw-headless --help` 冒烟 + 检查 API key + 测试连通性）。

---

## 六、界面优化建议

1. **结构化消息流**：废弃纯文本追加，改为「用户气泡 / AI 文本（markdown）/ Thinking 折叠卡片 / ToolCard（可展开）/ Timeline / Error 卡片」的条目化布局，视觉对齐 TUI `OutputView`。
2. **ToolCard 折叠**：>5 行结果默认折叠为单行摘要 + 「展开」按钮，展开最多 60 行（对齐 `tool_card.rs` 的 `COLLAPSE_THRESHOLD=5` / `MAX_EXPANDED_LINES=60`）。
3. **Thinking 折叠**：流式展开，完成后折叠为 `▶ Thinking (N chars)`，点击展开（对齐 `output_view.rs`）。
4. **状态栏信息密度**：右侧状态栏显示 `模型 · #turn · ⏳流式计时 · 💰cost · cwd`，与 TUI `status_bar.rs` 一致。
5. **侧边栏三区**：会话列表 / 当前 turn 工具历史 / 用量统计（token/cost/成功率），对齐 TUI `sidebar.rs`。
6. **主题适配**：所有颜色用 VS Code CSS 变量（`--vscode-*`），自动适配深/浅色主题（当前 HTML 已部分使用，需全面覆盖）。
7. **代码高亮**：工具结果中的代码块用 `highlight.js` 或 markdown-it 的 fence 高亮，超 50 行降级纯文本（对齐 TUI `tool_card.rs` 的 `SYNTAX_HIGHLIGHT_MAX_LINES=50`）。

---

## 七、配置流程简化方案

1. **binary 定位零门槛**：首次向导用文件选择器定位 `claw-plus-headless.exe`，写入 `claw.binaryPath` 绝对路径；不依赖 PATH（根治 GUI 进程 PATH 不一致的 ENOENT）。
2. **一键安装**：检测 binary 缺失时，提供「一键运行 install.ps1」按钮（现有 `runInstaller` 已有框架，需补真实脚本定位）。
3. **多 provider 引导**：向导按 provider 分步收集 API key，存入 SecretStorage（不进 settings.json 明文）；环境变量 `DEEPSEEK_API_KEY` 等自动预填。
4. **连通性自检**：向导完成后自动 spawn binary 做 `initialize` 冒烟 + API key 校验，失败给出可操作提示（而非运行时才报 "Transport not started"）。
5. **doctor 诊断入口**：命令面板新增 `Claw: Run Diagnostics`，输出 binary/API key/权限/工作区 4 项健康检查（对齐 TUI `/doctor`）。
6. **修掉已知 bug**：`--version` → `--help` 探测（已修）；spawn `error` 事件诊断（已修）；`main` 路径 `./out/src/extension.js`（已修）。

---

## 八、质量验收标准

### 8.1 P0 验收

- [ ] 发送 prompt 后，AI 回复**逐字流式**显示（首个 token < 500ms，后续 delta 连续，无整段等待）。
- [ ] markdown 标题/代码块/加粗/表格正确渲染，表格行不错位。
- [ ] 工具调用显示 ToolCard：执行中 ⏳，完成 ✓/✗，>5 行结果折叠可展开。
- [ ] Thinking 流式展开，完成后折叠为 `▶ Thinking (N chars)`，可点击展开。
- [ ] 状态栏显示：模型名、turn 计数、流式计时器、token 数、成本（USD/CNY 本地化）、cwd。
- [ ] cancel 按钮能中断进行中的 prompt（`StopReason::Cancelled`）。
- [ ] 现有 `cargo test -p claw-shell` 与 `npm test` 不回归。

### 8.2 P1 验收

- [ ] 可同时打开 ≥2 个 chat panel，各自独立 session，互不干扰。
- [ ] 能从历史会话列表恢复会话，历史消息可见。
- [ ] 能切换模型、切换 plan/act 模式、设置 reasoning effort。
- [ ] 会话分叉后新分支独立演进。
- [ ] 侧边栏展示会话列表 + 用量统计（token/cost/成功率）。
- [ ] `load_session`/`fork_session`/`set_session_mode`/`set_session_model` 单元测试通过。

### 8.3 P2 验收

- [ ] 切换活动编辑器 tab 时，agent 能感知当前文件并注入上下文。
- [ ] 编辑器诊断（LSP error/warning）自动作为上下文传递给 agent。
- [ ] 文件修改以 diff 视图展示（红删绿增）。
- [ ] `/` 触发 slash 菜单，fuzzy 搜索、Tab 补全。
- [ ] 首次向导：文件选择器定位 binary + provider 选择 + API key + 连通性自检，全程无报错。
- [ ] 端到端：新用户从安装到发第一条 prompt 全流程 < 5 分钟，无 "Transport not started" 类无上下文报错。

### 8.4 通用质量门槛

- [ ] 后端 `cargo build --workspace` 0 error（默认 `acp-0_10` feature）。
- [ ] 前端 `npm run compile` 0 error，`npm run lint` 0 error。
- [ ] 新增功能均有对应单元测试（Rust `#[cfg(test)]` + TS `test/suite`）。
- [ ] 性能：流式推送延迟 < 50ms（LaneEvent 发布 → IDE 收到），状态栏更新不阻塞 agent。

---

## 参考文件清单（探索依据）

**后端（Rust）**：
- [agent.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/agent.rs) — `ClawAgent`：prompt/cancel/new_session/load_session/set_session_mode
- [lane_bridge.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/lane_bridge.rs) — LaneEvent→ACP 桥接 + Session Bus
- [conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs) — `run_turn_async`、`AssistantEvent`、`with_hook_progress_reporter`/`with_tool_result_callback`
- [message.rs](file:///d:/claw-code-src/rust/crates/claw-acp/src/message.rs) — ACP 消息类型
- [headless.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/bin/headless.rs) — stdio 服务器入口

**前端（TypeScript）**：
- [extension.ts](file:///d:/claw-code-src/vscode-extension/src/extension.ts) — 扩展入口、命令注册、反向请求 handler 注册
- [chat-panel.ts](file:///d:/claw-code-src/vscode-extension/src/chat-panel.ts) — 聊天面板（渲染层需重写）
- [status-bar.ts](file:///d:/claw-code-src/vscode-extension/src/status-bar.ts) — 状态栏（需增强）
- [setup-wizard.ts](file:///d:/claw-code-src/vscode-extension/src/setup-wizard.ts) — 首次配置向导
- [handlers.ts](file:///d:/claw-code-src/vscode-extension/src/handlers.ts) — 三个反向请求 handler
- [types.ts](file:///d:/claw-code-src/vscode-extension/src/types.ts) — ACP 类型定义
- [acp-transport.ts](file:///d:/claw-code-src/vscode-extension/src/acp-transport.ts) — ACP 传输层

**TUI 参照**（功能清单来源）：
- `rust/crates/rusty-claude-cli/src/tui/` 全部文件（app.rs/sidebar.rs/status_bar.rs/tool_card.rs/input_line.rs/output_view.rs/slash_menu.rs）
- `rust/crates/rusty-claude-cli/src/app.rs`、`session_mgr.rs`、`streaming.rs`、`input.rs`
- `runtime/src/history_search.rs`、`runtime/src/usage.rs`

**设计文档**：
- [ide-integration-detail.md](file:///d:/claw-code-src/docs/modules/ide-integration-detail.md)
