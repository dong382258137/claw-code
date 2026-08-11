# TUI 窗口化渲染架构设计（瘦前端 · 流式渲染 + 文件重放回看）

日期：2026-08-11
状态：已实施（P1-P4 完成，claw.exe 已部署 08-11 04:25）
作者：Claw 工程

## 1. 背景与问题

### 1.1 触发事故
会话 `session-1786381203265`（D:\chanlunV2\chanlun_py）中，用户在一条 bash 卡片
（`[02:56:53] ├─ ✅ bash (1 行) │ [trimmed: 1843 bytes]`）之后，前端不再显示后续
AI 最终回复。排查结论：

- **后端完全正常**：`claw-diag.log` 显示 turn 完整跑完（iter=8 `text_len=1109`、
  `[turn-end] result=ok`，无 panic）；session JSONL 末尾有完整最终回复（621 字符）。
- **前端 buffer 是元凶**：`trim_if_needed()`（旧版策略"先删 Text 再裁 ToolCard"）
  在 buffer 超过 256KB 预算时把 AI 回复文本删光，前端"看起来坏了"。
- 该事故在 08-09 提交 `4400b1d2` 已修复（裁剪策略反转），但暴露的是**架构级问题**。

### 1.2 架构级缺陷
TUI 的 `OutputBuffer`（[output_view.rs](../rust/crates/rusty-claude-cli/src/tui/output_view.rs)）
承担了"无限历史存储 + 内存预算 + 增量渲染缓存"三重职责：

| 组件 | 职责 | 引入的复杂度/风险 |
|---|---|---|
| `entries: Vec<OutputEntry>` | 全量结构化存储 | 无限增长 |
| `text_total_bytes` + `trim_if_needed` | 256KB 内存预算 | 淘汰策略错误 → **内容被吞**（本次事故） |
| `cached_snapshot` / `rendered_lengths` | 渲染文本缓存 | 增量一致性 bug |
| `cached_lines` / `cached_lines_breaks` | 行缓存 + 索引 | `try_unwrap`/`expect` 退化 → **渲染卡住**（历史多次事故） |
| `recompute_snapshot_tail` | 增量重渲染 | 复杂边界条件（from_idx 越界等） |

这些复杂度**全部是为"在 TUI 内存里保存无限历史"服务的**，而历史数据的真正权威
是后端 session JSONL（`.claw/sessions/{id}/session-*.jsonl`，完整记录每个 message
的 blocks）。TUI 只是"展示信息的界面"，不该重复存储。

## 2. 目标架构

**原则：TUI 是瘦渲染器（View），数据权威在后端（session JSONL）。**

- **实时输出**：emitter 事件 → 渲染 → 追加到窗口 → 画屏。窗口滑出即丢弃。
- **历史回看**：用户滚动超出窗口 → 从 session JSONL 按需加载更早 message → 用
  **同一套渲染管线**流式重放 → 渲染到窗口。与实时输出共用渲染器。
- **内存恒定**：窗口大小固定（如 `MAX_WINDOW_LINES ≈ 2000` 渲染行），无预算、
  无淘汰、无跨窗口缓存一致性。

```
┌─────────────────────────────────────────────────────────────┐
│  实时路径:                                                    │
│  StreamingStatusEvent ─→ 渲染管线 ─→ window(VecDeque<Line>) ─→ draw │
│                     (markdown→ANSI→Line)    ↑ 尾部追加        │
│                                                     │ 滑出丢弃  │
│  回看路径:                                                    │
│  session JSONL ─→ SessionReplay ─→ 渲染管线 ─→ window 头部插入   │
│   (按需,滚动超出窗口时)              (同一渲染器)              │
└─────────────────────────────────────────────────────────────┘
```

## 3. 关键设计决策

### 3.1 窗口模型（替代"全量 entries + 三份缓存"）

`OutputBuffer` 重构为窗口化：

- `window_entries: VecDeque<OutputEntry>`：只保留窗口内条目（结构化，供 ToolCard
  更新 / 折叠 / 点击命中 / J-K-E 跳转）。
- 丢弃策略 = **固定容量 + 头部弹出**，无预算计算、无 trim 循环。
  弹出即"滚出窗口"，不需要代价：窗口固定小（条数 ≤ `MAX_WINDOW_ENTRIES ≈ 400`）。
- 渲染缓存简化为**一次性重建窗口内行**（窗口小，全量 `ansi_to_lines` 毫秒级），
  删除 `cached_snapshot`/`rendered_lengths`/`cached_lines_breaks` 的增量机制。
- `total_written`（draw 触发信号）改为单调版本号 `version: u64`，每次 append/
  push/complete 递增，语义不变。

### 3.2 历史回看 = 从文件重放（复用流式渲染管线）

新增 `SessionReplay`（[session_replay.rs](../rust/crates/rusty-claude-cli/src/tui/session_replay.rs)）：

- 读取 session JSONL 的 `type: message` 行，按 `blocks` 顺序流式产出 `OutputEntry`：
  - `text` → 走 `MarkdownStreamState` 增量渲染（与实时 TextDelta 同管线）
  - `thinking` → `OutputEntry::thinking`（摘要）
  - `tool_use` → `OutputEntry::tool_card_start`
  - `tool_result` → 按 `tool_use_id` 匹配并 `complete_tool_card`（JSONL 中
    tool_result 紧跟其 tool_use，顺序天然配对）
- 交互：用户 PgUp 滚动到窗口顶部再向上 → 触发 `replay_earlier(batch)`，把更早
  一批条目渲染后插入窗口头部，滚动位置相应偏移。
- 容量上限：单次加载不超过窗口上限（防止一次加载整个 2 小时会话）。

### 3.3 ToolCard 原地更新（窗口内的特例）

`complete_tool_card` 需要修改已渲染卡片（result 到达）。窗口化下这仍然成立——
因为卡片必须在窗口内才会被"完成"（result 事件与卡片创建在同一窗口时间窗内）。
若卡片已滚出窗口，`complete_tool_card` 找不到目标 → 按现有 `complete_tool_card_by_name`
兜底或忽略（此时卡片不在屏上，无感知）。

### 3.4 会话导出/文本接口

- `render_all()`/`buffer()`（供 turn 完成回显检查、粘贴等）改为基于窗口内容的
  轻量实现；检查 `ends_with('\n')` 之类的用途不受影响。
- `truncated()` 删除（不再有淘汰）。

## 4. 具体改动清单

### 4.1 删除（简化）
- `output_view.rs`：`text_total_bytes`、`trim_if_needed`、`MAX_BUFFER_BYTES`、
  `MAX_TRIM_ITERS`、`rendered_lengths`、`cached_snapshot`、`cached_lines_breaks`、
  `invalidate_lines_cache`、`recompute_snapshot_tail`（增量版本）、`truncated`。
- 相关测试：`trim_*`、`buffer_trims_*`、`total_written_*` 等适配新语义。

### 4.2 新增
- `tui/session_replay.rs`：JSONL → OutputEntry 流（含 tool_use/tool_result 配对）。
- `OutputBuffer::window_capacity` 常量 + 头部弹出逻辑。
- `OutputBuffer::version()` 单调版本号。

### 4.3 修改
- `app.rs`：滚动逻辑——滚动到窗口顶时请求 `replay_earlier`；`snapshot_lines()`
  /`snapshot_breaks()` 改为窗口内轻量实现。
- `output_view.rs`：`snapshot_lines` 窗口全量重建（去掉 Arc 缓存增量优化）。

## 5. 兼容性与回归

- **交互能力保持**：Ctrl+T 折叠、鼠标点击切换、J/K 跳转回复、E 跳错误、
  PgUp/PgDn 滚动、sticky header——全部基于窗口内条目，语义不变（仅限窗口范围）。
- **draw 触发**：`version` 语义与 `total_written` 一致，主循环无感知。
- **测试**：重写 output_view 单测为新窗口语义；新增 `SessionReplay` 单测
  （用真实 JSONL 片段验证配对渲染）；保留本次事故回归测试
  （窗口满 + 最终回复流式 append → 内容完整）。

## 6. 分阶段计划

| 阶段 | 内容 | 状态 |
|---|---|---|
| P1 止血 | trim 策略反转 + 回归测试 | ✅ 事故场景不再复现 |
| P2 窗口化 | OutputBuffer 窗口化 + 删缓存/预算 | ✅ `VecDeque` 窗口(400 条) + version 号，内存恒定 |
| P3 回看 | SessionReplay + 滚动触发加载 | ✅ `session_replay.rs` 文件重放 + 滚动到顶自动加载 |
| P4 收尾 | 测试适配 + 部署 + 观察 | ✅ 536 测试全绿，claw.exe 部署 08-11 04:25 |

## 7. 验收标准

1. 2 小时长会话（≈70 工具调用 + 多次流式回复）TUI 内存恒定（窗口上限内），
   draw 流畅，无"内容被吞/前端卡住"。
2. 翻历史到窗口之外能从 session 文件加载并正确渲染（含 ToolCard 配对）。
3. `cargo test -p rusty-claude-cli` 全绿。
4. 本次事故回归测试（`repro_session_tail_stream_final_reply_after_trim`）持续通过。
