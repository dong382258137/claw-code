# 会话互通（Session Bus）设计文档

> **持久化文档** — 本方案固化到文件以绕过会话上下文限制。所有改动点带 `文件:行号` 锚定，便于 subagent 独立执行时无需重新调研。
>
> 状态：Epic 0-4 已实施（2026-08-12）

**Goal:** 让 CLAW 的多个会话窗口（TUI 主会话 / 子代理会话 / IDE 多面板 / IM 频道）之间**主动可见**（互相看到状态、未读、产出）并**互通交流**（互发消息、交接结果），形成统一的事件总线。

**Architecture:** 新增 `SessionBus` 作为会话级发布/订阅中枢。进程内用 `broadcast::Sender` + peer 注册表实现零拷贝路由；跨进程用 `.claw/bus/` 文件事件队列（原子写 + watcher）与 ACP 协议扩展（`session/broadcast` + `SessionUpdate::PeerMessage`）。会话内已有的 `MultiAgentCoordinator` / `SubagentHandoff` / `lane_events` / `memory` 全部复用为总线"字节"，不另起炉灶。

**Tech Stack:** Rust, tokio (sync/mpsc/broadcast), serde_json, notify(文件 watcher, 与 hooks.rs 同库), ACP(agent_client_protocol)。

---

## 1. 现状与差距

### 1.1 会话窗口现状（彼此隔离）

| 会话形态 | 实现位置 | 互通能力 |
|---|---|---|
| TUI 主会话 | [session_mgr.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/session_mgr.rs)（`/session` CRUD、resume、fork、picker） | 单活跃会话，一次只能看一个 |
| 会话持久化 | [session_control.rs](file:///d:/claw-code-src/rust/crates/runtime/src/session_control.rs)（按 workspace fingerprint 分区） | 跨 CWD 会话互相不可见 |
| VS Code 多面板 | [chat-panel.ts](file:///d:/claw-code-src/vscode-extension/src/chat-panel.ts)（每个 panel 一个独立 ACP session） | 只有 `cancelAll()`，无消息互通 |
| IM 频道 | [session.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs#L395-L436)（每 ChatKey spawn 独立 agent，通知流合并到一条 channel） | 频道间无通信，仅共享 hub |

**结论：顶层会话之间没有任何"主动可见 + 互通"通道。**

### 1.2 会话内互通（已成熟，可直接复用）

同一主会话内的 subagent 已经具备完整互通基础设施：

| 能力 | 实现位置 | 复用方式 |
|---|---|---|
| 子代理状态机 | [multi_agent/mod.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/mod.rs#L363-L446)（`spawn`/`spawn_with_model`） | 会话注册/注销的状态来源 |
| Teammate 黑板 | [task_registry.rs](file:///d:/claw-code-src/rust/crates/runtime/src/task_registry.rs)（`spawn_subagent_for_task`/`complete_subagent`） | 现有跨 agent 数据交换范本 |
| 结构化 handoff | [handoff.rs](file:///d:/claw-code-src/rust/crates/runtime/src/multi_agent/handoff.rs)（`.claw/subagents/{id}.md` YAML frontmatter） | 跨进程消息的持久化格式 |
| 全局事件流 | [lane_events.rs](file:///d:/claw-code-src/rust/crates/runtime/src/lane_events.rs)（23 种事件 + `drain_lane_events`） | 总线状态事件的来源 |
| 事件→ACP 通知 | [lane_bridge.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/lane_bridge.rs) | 跨进程推送的现有实现范本 |
| 共享知识库 | [memory_store.rs](file:///d:/claw-code-src/rust/crates/runtime/src/memory_store.rs) / `memory_semantic.rs` | 会话间知识交换落点 |
| 磁盘共享落点 | `.claw/history.db`（FTS5）、`.claw/decision_log.db`、`.claw/tool_results_archive.jsonl` | 已证实的跨会话磁盘共享模式 |
| 跨进程 HTTP hub 候选 | [server.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L29-L77)（axum，`listen_addr` 默认 `127.0.0.1:3456`） | Epic 3 的 hub 宿主 |

### 1.3 差距清单

1. 无统一 peer 注册表（不知道"有哪些会话在跑、状态如何"）。
2. 无消息路由（不知道"发给谁"）。
3. 无跨进程消息通道（ACP 无 broadcast，文件层无事件队列）。
4. TUI Sidebar 无对等会话视图（[sidebar.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui/sidebar.rs#L125-L148) 现有 `render_session_section` 只显示当前会话的 cwd/版本，无 peer 列表）。
5. OutputView 无 peer 消息条目类型（[output_view.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui/output_view.rs) 的 `OutputEntry` 无 `PeerMessage` 变体）。

---

## 2. 架构设计

### 2.1 核心：SessionBus（runtime 新增模块 `src/session_bus.rs`）

```rust
/// 会话总线 —— 会话级发布/订阅中枢。
pub struct SessionBus {
    /// 对等会话注册表。
    peers: Arc<Mutex<HashMap<String, BusPeer>>>,
    /// 进程内广播通道。
    tx: tokio::sync::broadcast::Sender<BusMessage>,
}

pub struct BusPeer {
    pub session_id: String,
    pub label: String,          // "TUI 主会话" / "subagent:xxx" / "ide:panel-1" / "im:feishu:chat-123"
    pub kind: PeerKind,         // Main | Subagent | Ide | Im
    pub status: PeerStatus,     // Idle | Streaming | Blocked | Done
    pub unread: u32,            // 发给本 peer 但未读的消息数
    pub last_seen_ms: u128,
    pub config_path: Option<PathBuf>, // 跨进程 peer 的配置文件/事件队列路径
}

pub struct BusMessage {
    pub from: String,
    pub to: String,             // 目标 session_id 或 "*"（广播）
    pub kind: BusMessageKind,   // State | Message | Handoff | Command
    pub payload: serde_json::Value,
    pub hop: u8,                // 防循环：> MAX_HOP(3) 丢弃
    pub ts_ms: u128,
}
```

接口（全部幂等、可重放）：
- `register(peer) -> Result<(), String>` / `leave(session_id)`
- `publish(msg) -> Result<Vec<String>, String>`（返回实际送达的 peer 数）
- `peers_snapshot() -> Vec<BusPeer>`（供 Sidebar / `/bus list` 查询）
- `update_status(session_id, status)`（由 StatusEmitter 周期性调用）

### 2.2 会话内接入点

| 接入点 | 位置 | 职责 |
|---|---|---|
| 主会话注册 | `rusty-claude-cli/src/app.rs`（REPL/ TUI 启动处） | `bus.register(Main)`，退出时 `leave` |
| subagent 注册 | [conversation.rs:4066](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs#L4066) `execute_dispatch_subagent` | 派发时 `bus.register(Subagent)`，完成时发布 `Handoff` 摘要 |
| 状态发布 | `StatusEmitter`（TUI cumulative usage 唯一真相源） | 每轮 turn 开始/结束 `update_status(Streaming/Idle)` |
| 命令入口 | [commands/src/lib.rs](file:///d:/claw-code-src/rust/crates/commands/src/lib.rs)（slash command 注册处） | 新增 `/bus send/list/watch` |

### 2.3 TUI 集成

1. **Sidebar 对等会话区**：[sidebar.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui/sidebar.rs#L125-L148) `render_session_section` 扩展为列出所有 `peers_snapshot()`：
   - 状态 badge：`Streaming`（brand 色）/ `Idle` / `Blocked`（warning）/ `Done`。
   - 未读计数：`[3]` 徽标，复用 auto-follow 的 `[↓ N 行新输出]` 提示模式（见 project_memory L2）。
2. **OutputEntry::PeerMessage**：[output_view.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui/output_view.rs) 新增变体，渲染为 `[来自 subagent:xxx] <摘要>`，优先级 P1（折叠档）。
3. **命令**：
   - `/bus list` — 列出全部 peer 与状态。
   - `/bus send <target> <text>` — 主会话向目标 peer 发消息。
   - `/bus watch <target>` — 订阅某 peer 的消息流（进入 OutputView）。

### 2.4 跨进程通道（Epic 2/3）

- **文件事件队列**（首选，零新运行时依赖）：
  - 写：`publish()` 对未注册的直接目标或广播目标，写入其邮箱 `.claw/bus/{target}/{ts}-{pid}-{seq}.json`（原子写：`.tmp/` 暂存 + rename，沿用 [project_memory L3] Windows 临时文件教训）。
    - **实现偏差（比原设计更安全）**：一次消息 = 一个独立文件，而非追加写 `.jsonl`——规避跨进程并发追加同一文件导致的 Windows 文件锁/数据交错。
  - 读：`SessionBus::start_mailbox_poller` 后台线程每 500ms 消费本会话邮箱，解析 `BusMessage` 后 `inject` 注入本地总线（发送端已做权限校验，注入跳过重检）。
  - 消费确认：读完 rename 到 `.claw/bus/.done/{target}/`（幂等，防重复投递）。
  - 远端发现：`remote_peers()` 扫描 `{bus_root}` 子目录，`/bus list` 展示 `remote:<id>`。
- **ACP 协议扩展**（`claw-shell`，复用官方 `ExtNotification` 扩展通道，不侵入外部 `agent-client-protocol` crate）：
  - 面板 → 总线：`ExtNotification("session/broadcast")`，params = `{from, to, kind, text}`，经 `lane_bridge::handle_broadcast_notification` 注册面板为 Ide peer + `publish`。
  - 总线 → 面板：`ExtNotification("session/peer_message")`，params = `BusMessage` JSON，经 `lane_bridge::push_bus_message_to_acp` 推送（复用 [lane_bridge.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/lane_bridge.rs) 的 fire-and-forget 模式），面板未读在 turn 结束时 drain。
  - **实现偏差**：原设计拟新增 `BroadcastRequest` + `SessionUpdate::PeerMessage`，但 `SessionUpdate` 属外部 `agent-client-protocol` crate（无法本地扩展），改用官方扩展通道实现等价语义。
- **IM hub**（Epic 3）：在 [server.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs#L50-L77) 增加 `POST /api/bus/send`，让不同 IM 频道通过 hub 互发。

### 2.5 安全与防循环

- **默认拒绝**：`session_bus.allow` 配置显式列出可互通的目标（`*` = 全通）。未配置时仅允许 `Main → Subagent`（子代理是主会话派生的，属同一信任域）。
- **防循环**：`hop > 3` 丢弃（扩展 [loop_detection.rs](file:///d:/claw-code-src/rust/crates/runtime/src/loop_detection.rs) 的检测器，BusMessage 自带 hop）。
- **限流**：单 peer 每秒最多 20 条，超出排队（复用 MultiAgent 并发限流 ≤5 的实践）。

---

## 3. 改动清单（文件级）

| 文件 | 改动 |
|---|---|
| `runtime/src/session_bus.rs` | **新建**：BusPeer/BusMessage/SessionBus（注册/路由/状态/文件队列写） |
| `runtime/src/lib.rs` | re-export `session_bus` 模块 |
| `runtime/src/loop_detection.rs` | 增加 hop 抑制分支 |
| `runtime/src/conversation.rs` | `execute_dispatch_subagent` 中注册 subagent peer + 完成时发布 Handoff |
| `rusty-claude-cli/src/tui/sidebar.rs` | `render_session_section` 渲染 peer 列表 |
| `rusty-claude-cli/src/tui/output_view.rs` | `OutputEntry::PeerMessage` 变体 |
| `commands/src/lib.rs` | 注册 `/bus send/list/watch` |
| `claw-acp/src/message.rs` | 新增 `BroadcastRequest` + `SessionUpdate::PeerMessage` |
| `claw-shell/src/lane_bridge.rs` | 转发 `PeerMessage` notification（跨进程） |
| `rusty-claude-cli/src/app.rs` | 主会话注册/注销 |

---

## 4. 实施阶段（Pilot 优先）

### Epic 0（Pilot，纯进程内，最低风险）

**范围**：`SessionBus` 进程内路由 + **主会话 ↔ subagent 互通** + Sidebar peer 视图 + `/bus list`。

**验证目标**：主会话派发 3 个并行 subagent（复用 `execute_dispatch_subagent`），任意 subagent 完成时发布 Handoff 摘要 → 主会话 OutputView 出现 `[来自 subagent:xxx]` 条目，Sidebar 显示全部 subagent 状态（Streaming→Done）。**不引入任何跨进程协议。**

改动面：`session_bus.rs` + `conversation.rs` + `sidebar.rs` + `output_view.rs` + `/bus list`。

### Epic 1（进程内完善）

- `/bus send <target> <text>`：主会话主动向 subagent 发消息（复用 `task_registry` steer 通道）。
- `/bus watch <target>`：订阅流进入 OutputView。
- 未读计数徽标 + 状态 badge。
- `session_bus.allow` 权限配置。

### Epic 2（跨进程：VS Code 多面板互通）

- ACP 新增 `BroadcastRequest` + `PeerMessage`。
- `.claw/bus/` 文件事件队列（原子写 + notify watcher + `.done/` 消费确认）。
- VS Code extension 面板通过 `session/broadcast` 互发（在 [chat-panel.ts](file:///d:/claw-code-src/vscode-extension/src/chat-panel.ts) 增加广播 UI）。

### Epic 3（IM 频道互通）

- [server.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs) 增加 `/api/bus/send` + `/api/bus/subscribe`。
- IM 频道可用 `/bus send <channel>` 跨频道协作。

---

## 5. 测试策略（TDD）

| 层 | 测试 | 断言 |
|---|---|---|
| unit | `session_bus.rs` 注册/注销/路由 | `publish(to="*")` 送达全部；`to=具体id` 只送达目标；注销后不再送达 |
| unit | 未读计数 | 消息送达未消费 → `unread+1`；watch 后清零 |
| unit | hop 抑制 | `hop=3` 再转发被丢弃 |
| unit | 权限 | 未配置 `allow` 时 `Im→Im` 被拒，`Main→Subagent` 放行 |
| unit | 文件队列 | 原子写 + rename 消费后无重复投递 |
| TUI | `render_session_section` 快照 | peer 列表含状态 badge 与未读徽标（沿用 [sidebar.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/tui/sidebar.rs#L432-L550) 现有测试模式） |
| integration | 主会话派发 3 并行 subagent → 完成即广播 | mock ApiClient 下主会话收到 3 条 `PeerMessage`，顺序与完成顺序一致 |

---

## 6. 实现可行性清单（9 项）

| # | 检查项 | 结论 |
|---|---|---|
| 1 | **代码事实锚定** | 所有改动点已锚定 `文件:行号`，见 §1/§2.2/§3 |
| 2 | **依赖完整性** | `tokio sync/broadcast` 已启用（[runtime/Cargo.toml](file:///d:/claw-code-src/rust/crates/runtime/Cargo.toml)）；`notify` 已在 hooks 使用；**无需新增重型依赖** |
| 3 | **编译可行性** | 无循环依赖：`session_bus` 只依赖 `serde_json` + `tokio`，conversation.rs 单向引用它 |
| 4 | **测试覆盖** | 每个新行为有单测 + TUI 快照 + 1 条集成链路（§5） |
| 5 | **平台兼容（Windows）** | 文件队列用 `target/tmp/` + rename 原子写（[project_memory L3] 已验证模式）；avoid 跨进程锁 |
| 6 | **并发与状态安全** | `broadcast::Sender` 线程安全；`peers` 用 `Mutex` + `.unwrap_or_else(|e| e.into_inner())`（project_memory L2 规则） |
| 7 | **性能与内存** | 广播 O(peer 数)；`peers_snapshot()` 只读拷贝；文件队列按行消费无整文件读 |
| 8 | **向后兼容** | `/bus` 为新增命令；`OutputEntry` 新变体对旧快照/重放逻辑加 match 分支即可（windowed renderer 已结构化）；现有会话行为不变 |
| 9 | **可回滚** | `session_bus.allow` 默认仅 `Main→Subagent`，配置可整体关闭；Epic 0 纯进程内，失败不伤任何现有链路 |

---

## 7. 风险与对策

| 风险 | 对策 |
|---|---|
| 跨进程消息丢失（进程崩溃） | 文件事件队列持久化 + `.done/` 消费确认 + 幂等重放 |
| 消息风暴 / 广播放大 | hop 上限 3 + 单 peer 限流 20 msg/s |
| 会话内容泄露（IM/IDE peer） | `deny by default` + `session_bus.allow` 显式白名单 |
| Windows 文件锁 | `target/tmp/` 原子写 + rename（已在 [project_memory L3] 验证） |
| TUI buffer 污染 | `PeerMessage` 走结构化 `OutputEntry`，不拼接进文本流 |

---

## 8. 验收标准

1. `/bus list` 在 TUI 中列出主会话 + 全部活跃 subagent 及状态。
2. subagent 完成时主会话 OutputView 出现 `[来自 subagent:xxx]` 摘要条目，Sidebar 未读 +1。
3. 并行派发 3 个 subagent，三个完成事件全部到达，顺序正确，无丢失。
4. `session_bus.allow` 为空时 `Main→Subagent` 可用，`Im→Im` 被拒。
5. 全程无 panic、无 `expect`（遵循 project_memory L2 Mutex 规则）。

---

## 9. 审查修复记录（2026-08-11）

| 级别 | 发现 | 修复 |
|---|---|---|
| P1 | **unread 队列无界增长**：TUI 尚未消费 bus 消息，长会话下 `publish` 每投递一条 push 一条，无消费方 `mark_read` → 内存泄漏 | 新增 `MAX_UNREAD_PER_PEER = 100`，超限丢弃最旧消息，unread 计数保持精确；单测 `unread_queue_is_bounded` |
| P2 | **Main peer 标签**："TUI 主会话"在 REPL 模式下名不副实 | 改为"主会话" |

**已解决（Epic 1）**：
- `hop` 字段当前不递增（进程内 `publish` 直接路由、无转发路径，无循环风险；跨进程转发方需负责 `hop+1`）。
- `/bus send`（Subagent 目标经 steer 通道注入；未注册目标经文件队列投递）、`/bus watch` / `unwatch`（watch 镜像 + TUI/REPL drain 显示）、`session_bus.allow` 权限配置已实现。

**待实施（Epic 3）**：IM hub（`POST /api/bus/send` + `/api/bus/subscribe` on im-bridge server）。

**Epic 3 完成记录（2026-08-12）**：
| 项 | 实现 | 验证 |
|---|---|---|
| IM 频道注册 | [session.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs) `register_im_peer`：频道首次建会话时注册为 `Im` peer（id=`im:{platform}:{chat_id}`）；idle cleanup / `/new` 时 `leave_im_peer` | session 单测 `chat_key_bus_peer_id_format` |
| `/bus` 命令 | [commands.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/commands.rs) 新增 `ChatCommand::Bus{args}`；[session.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/session.rs) `handle_bus_command` 执行 list/send/watch/unwatch（send 复用 `bus_send_and_push`：publish + RouteTarget IM 直发） | commands 5 单测（list/send/watch/unwatch/非bus误匹配） |
| HTTP hub API | [server.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs) `POST /api/bus/send`（外部进程自动注册 Ide peer）+ `GET /api/bus/list` + `GET /api/bus/poll?session_id=X`（轮询未读 + 消费确认） | — |
| hub 权限 | `run_server` 初始化 `set_allow(im:im)` + `ide:im` + `ide:ide`（保持 deny-by-default 其余） | server 3 单测（im→im 互通 / ide→im 放行 / 外部自动注册） |
| bus 路由同步 | `process_feishu_message` / `handle_im_message` 建 RouteTarget 时同步 `register_bus_route` | 编译 + 回归 |
| 回归 | im-bridge 29 全绿（2026-08-12） | — |

**Epic 1-2 完成记录（2026-08-12）**：
| 项 | 实现 | 验证 |
|---|---|---|
| `/bus send` | commands_handler `run_bus_send`：Subagent → Command(steer)；其他 → Message；`*` 广播 | CLI 4 单测 + 手工 |
| `/bus watch` | `SessionBus::watch` + publish 镜像（`watch_relay`）+ TUI 事件循环 drain + REPL 循环 drain | session_bus 7 单测（镜像/权限/幂等） |
| `session_bus.allow` | config `sessionBus.allow` 解析 + 校验 schema + `apply_allow_rules`（`*` 通配） | config 2 单测 + session_bus 2 单测 |
| 文件事件队列 | `.claw/bus/{target}/{ts}-{pid}-{seq}.json` 原子写 + `.done/` 归档 + 500ms poller 注入 | session_bus 6 单测（含跨进程双实例端到端） |
| ACP 互通 | `session/broadcast` 处理 + `session/peer_message` 推送 + 面板注册 Ide peer | lane_bridge 3 单测 |
| 回归 | runtime 1714 / claw-shell 44 / CLI lib 544 / commands 46 全绿（2026-08-12 复核）；claw.exe full-tui 零 warning 编译 | — |

**Epic 4 统一路由层完成记录（2026-08-12）**：

背景：Epic 0-3 落地后存在三套并行的跨进程入口——CLI 文件事件队列、IDE ACP 扩展（lane_bridge）、IM HTTP hub（server.rs），其中外部发送方注册 / kind 映射 / BusMessage 构造 / 发布逻辑在各端重复实现；且设计文档 §2.5 的限流要求（单 peer 20 msg/s）未落地。

**方案**：在 `SessionBus` 上沉淀统一外部入口 API，三套通道全部收敛为"注册身份 + 发布文本"两个调用，重复逻辑与限流集中到单一路由层。

| 项 | 实现 | 验证 |
|---|---|---|
| 统一入口 API | [session_bus.rs](file:///d:/claw-code-src/rust/crates/runtime/src/session_bus.rs) 新增 `BusMessageKind::from_str`、`BusMessage::text`/`text_payload`、`SessionBus::ensure_external_peer`（幂等注册 Ide）、`SessionBus::publish_text`（构造 + 发布 + 限流） | session_bus 5 单测（幂等注册/kind 映射/构造器/权限+限流/按发送方隔离） |
| 限流 | `MAX_PUBLISH_PER_SEC = 20`，`publish_times` 滑动窗口（1s），按 `from` 隔离 | 同上（超限 Err、不同 from 互不影响） |
| IDE ACP 收敛 | [lane_bridge.rs](file:///d:/claw-code-src/rust/crates/claw-shell/src/lane_bridge.rs) `handle_broadcast_notification` 改为 `ensure_external_peer` + `publish_text`，删除重复注册/映射/构造 | claw-shell 44 全绿 |
| IM HTTP 收敛 | [server.rs](file:///d:/claw-code-src/rust/crates/im-bridge/src/server.rs) `bus_send_handler` 改为 `ensure_external_peer` + `publish_text`，删除重复注册/映射/构造 | im-bridge 29 全绿 |
| 回归 | runtime 1719 / claw-shell 44 / im-bridge 29 / CLI lib 544 全绿（2026-08-12） | — |

**Epic 5 AI 总线工具完成记录（2026-08-12）**：

背景：Epic 0-4 使总线能力对用户命令（`/bus`、IM `/bus` 命令）和外部进程开放，但框架内 AI 无法自主调用——tools crate 无任何 bus 工具，AI 只能被动接收路由结果。

**方案**：以"工具 + 教程 + 约束"三层落地，让 AI 不仅可用、且知道何时用、如何用：
1. **RuntimeToolDefinition 注册**（[plugin_state.rs](file:///d:/claw-code-src/rust/crates/rusty-claude-cli/src/plugin_state.rs)，Phase 4-B 后）：`bus_list`（ReadOnly，无参数）、`bus_send`（ReadOnly，`to`/`text` 必填）、`bus_watch`（ReadOnly，`target` 必填 + `unwatch` 可选），domain_tags `session-bus` / `orchestration`，LLM 可发现。
2. **run_turn 拦截执行**（[conversation.rs](file:///d:/claw-code-src/rust/crates/runtime/src/conversation.rs)）：`else if tool_name == "bus_*"` 分支路由到 `execute_bus_list` / `execute_bus_send` / `execute_bus_watch`——send 到 Subagent 转 `Command(steer)`，其余转 `Message`；watch 镜像到本会话 unread 视图。
3. **System Prompt 教程段**（[prompt.rs](file:///d:/claw-code-src/rust/crates/runtime/src/prompt.rs) `get_session_bus_section`，挂接在 `get_agent_subagent_types_section` 之后）：表格说明三工具适用场景 + Workflow（dispatch 后先 `bus_list` 确认注册 → 需转向用 `steer_subagent`/`bus_send` → 追踪输出用 `bus_watch`）+ Constraints 强制约束（target 必须来自 `bus_list`、遵循 `session_bus.allow` 权限边界、禁止 watch 自身）。

| 项 | 实现 | 验证 |
|---|---|---|
| 工具注册 | plugin_state.rs 3 个 `RuntimeToolDefinition` | cargo check 通过 |
| 拦截执行 | conversation.rs 3 分支 + 3 execute 方法 | bus_tool 6 单测 |
| 教程段 | prompt.rs `get_session_bus_section`（表格 + Workflow + Constraints） | runtime 1723 全绿 |
| 回归 | runtime 1723 / claw-shell 44 全绿（2026-08-12） | — |

**全量审计修复记录（2026-08-12）**：

对多会话系统（Session Bus 核心 + AI 总线工具 + IM hub + IDE ACP 通道 + 文件事件队列）做全量测试与审计，发现并修复 5 项 BUG：

| # | 问题 | 修复 |
|---|---|---|
| 1 | `leave()` 注销 peer 时不清理 watch 订阅 → watched_peers 返回 stale id、同名重注册复活旧订阅 | leave() 同时清理 watcher 条目 + target 引用 + 空集合；测试 `leave_cleans_watch_subscriptions` |
| 2 | `publish_times` 限流表 peer 注销后残留 → 外部动态 session_id 长期运行内存增长 | leave() 同步清理限流窗口 |
| 3 | subagent 消费 steer/kill 用 `mark_read` 全清队列 → 混在队列里的普通 Message 被静默丢弃 | 新增 `consume_commands` API（只取 Command 保留 Message，锁序 peers→unread 防死锁），消费端接入；测试 `consume_commands_preserves_regular_messages` |
| 4 | 广播 `*` 自回显：publish 投递给发送者自身 → TUI drain 显示"来自自己"、IM 频道重复收到自己发出的消息 | dispatch 广播排除 sender（显式 to=自身仍投递）；`publish_to_star_delivers_all_allowed` 与 Handoff 测试断言同步更新 |
| 5 | `start_mailbox_poller` 注释称注入走 publish（含权限校验），实际走 inject（跳过） | 注释修正为与实现一致 |

**已确认不改（设计使然/低风险）**：IM→Main 默认拒绝（deny-by-default，需 `session_bus.allow=["im:main"]` 显式放行）；hub 广播 `*` 不直发真实 IM（仅具体 `im:` 目标直发）；`/api/bus/send` 无认证（默认 127.0.0.1 回环）；`bus_send/bus_watch` 保持 ReadOnly（跨会话发送受 session_bus.allow + 限流兜底）；IDE 广播空 text 不校验。

回归基线（修复后）：runtime 1725 / im-bridge 28 / claw-shell 44 全绿。

**问题重评估与第二轮修复（2026-08-12）**：

对首轮"已确认不改"项重新评估，发现关键事实：**im-bridge 进程从未接入文件事件队列**（无 `set_bus_root`、无 poller）——跨进程 IM 互通链路实际是断的，且 im-bridge.toml 无 `session_bus.allow` 配置入口（`apply_allow_rules` 仅 TUI 进程调用）。据此升级处理：

| # | 问题 | 修复 |
|---|---|---|
| 1 | **IM→Main 跨进程反向通道不通**：im-bridge 侧 `Im→Ide` 默认拒绝（文件路由目标 kind=Ide），且无配置入口，IM 用户 `/bus send` 到 TUI 主会话连文件都不写 | im-bridge `run_server` 追加 `set_allow(Im, Ide, true)`；`format_bus_send_result` 提示从误导的"目标未注册，仅写入总线"改为"目标不可达（未注册或权限拒绝）" |
| 2 | **hub 广播 `*` 不直发真实 IM**（功能缺口） | im-bridge 接入 Session Bus 文件事件队列：新增 `bus_root` 配置字段（指向 TUI 的 `.claw/bus`）；`run_server` 配置后 `set_bus_root` + 启动 `start_bus_mailbox_poller`；`register_im_peer` 创建频道邮箱；poller 消费频道邮箱 → 注入本地总线 → 目标为本 hub IM 频道时 `push_im_route` 直发真实 IM |
| 4 | **IDE 空 text 不校验** | `handle_broadcast_notification` 空/纯空白 text 返回 0 |
| 5 | **Done 子代理 peers 表无上限** | 新增 `MAX_DONE_SUBAGENTS=200` + `prune_done_peers`（按 last_seen_ms 淘汰最旧 Done，保留 Streaming/Main；完整记录仍在 coordinator/handoff 文件）；`BusPeerDoneGuard` Drop 时触发 |

**跨进程互通拓扑（配置后）**：
- TUI → IM：TUI 进程广播/定向 → 文件路由写 IM 频道邮箱 → im-bridge poller 消费 → `inject` + `push_im_route` 直发真实 IM ✅
- IM → TUI：IM 用户 `/bus send main:xxx` → `Im→Ide` 放行 → 文件路由写 TUI 主会话邮箱 → TUI poller 消费注入 ✅
- 配置方式：`~/.claw/im-bridge.toml` 增加 `bus_root = "<TUI 项目目录>/.claw/bus"`。

**仍保持现状（已记录）**：`/api/bus/send` 无认证（默认 127.0.0.1 回环；若改 `0.0.0.0` 则成为无认证远程总线写入 API，`Ide→Im` 放行可被外部伪造消息注入 IM 群，需警惕）；`bus_send/bus_watch` 保持 ReadOnly（跨会话发送受 session_bus.allow + 限流兜底）。

回归基线（第二轮修复后）：runtime 1726 / im-bridge 29 / claw-shell 45 全绿。
