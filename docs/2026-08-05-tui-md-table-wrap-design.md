# TUI 输出渲染修复：Markdown 表格对齐 + 换行可读性

> 日期：2026-08-05
> 状态：设计评审
> 关联：`rust/crates/rusty-claude-cli/src/render.rs`、`src/tui/app.rs`、`src/streaming.rs`

## 1. 问题描述

用户反馈 TUI 输出内容 MD 渲染存在两类问题：

1. **表格没有对齐、内容不分行导致表格错乱**：长内容单元格（URL / 路径 / 长句）使表格超出内容区宽度，TUI 在单元格中间硬折行，边框与列完全错位。
2. **无表格的输出内容换行不正确**：英文单词 / URL 在中间被拆断，阅读费力。

### 复现证据（`cargo run --example repro_md`，临时示例已删除）

场景 1 — 60 列下长 URL 单元格（表格自然宽 120+ 列）：

```
│ 工具       │ 说明                                         ← 第一行被终端宽度截断
                                             │              ← 孤立的右边框
│ read_file  │ 读取 https://raw.githubusercontent.com/exampl
e/very/long/path/to/a/documentation/file.md │               ← 单元格中间断行
```

场景 3 — 单词/URL 被拆断：

```
supercalifragilisticexpialidoc
ious 和                                    ← "supercalifragilisticexpialidocious" 被拆
https://example.com/some/p
ath 混合显示。                              ← URL 路径被拆
```

## 2. 现状分析（代码事实核查）

| 组件 | 位置 | 现状 | 验证 |
| --- | --- | --- | --- |
| `TableState` | render.rs:126-134 | 收集 headers/rows/current_cell，`push_cell` 做 trim | ✅ 已读 |
| `RenderState` | render.rs:152-163 | 含 `table: Option<TableState>`，无宽度字段 | ✅ 已读 |
| `render_markdown` | render.rs:272-290 | 逐事件驱动渲染，`trim_end` 收尾 | ✅ 已读 |
| `markdown_to_ansi` | render.rs:295-297 | 直接转发 `render_markdown` | ✅ 已读 |
| `render_event` 表格事件 | render.rs:407-444 | Start/End Table/TableHead/TableRow/TableCell | ✅ 已读 |
| `render_table` | render.rs:518-566 | 列宽 = 每列 `visible_width` 最大值，**无上限**；分隔行 `─*(w+2)` 用 `┼` 连接 | ✅ 已读 |
| `render_table_row` | render.rs:568-585 | `line.push_str(cell)` 直接拼整行；padding = `width - visible_width(cell)` | ✅ 已读 |
| `visible_width` | render.rs:923-930 | unicode-width + strip_ansi，CJK 正确 | ✅ 已读 |
| `strip_ansi` | render.rs:932-948 | 剥 CSI 序列 | ✅ 已读 |
| `wrap_line_to_display_lines` | tui/app.rs:310-397 | 按 grapheme 宽度折行，**无词边界感知** | ✅ 已读 |
| `wrap_lines_with_breaks` | tui/app.rs:446-475 | 按 entry 分组 wrap，记录 display breaks | ✅ 已读 |
| draw 循环 wrap 调用 | tui/app.rs:1014-1021 | `content_width = main_area.width`，wrap 缓存 (ptr+width) | ✅ 已读 |
| emitter 流式渲染 | tui/app.rs:2892-2903, 3030 | `MarkdownStreamState::push/flush` 渲染 ANSI 片段 | ✅ 已读 |
| `MarkdownStreamState` | render.rs:628-652 | `pending: String`，无宽度概念 | ✅ 已读 |
| streaming.rs 非 TUI 流式 | streaming.rs:513, 648 | 同用 `MarkdownStreamState::push` | ✅ 已读 |
| `wrap_plain_text` | tui/app.rs:409-437 | 输入框折行（光标对齐依赖），**不在本次范围** | ✅ 已读 |
| ToolCard 结果渲染 | tui/tool_card.rs:136-146 | 用 `highlight_code`（非 markdown 表格），不受影响 | ✅ 已读 |

关键行为事实：

- `Event::SoftBreak` → `append_raw("\n")`，若发生在表格内会写入 `current_cell`；实测 pulldown-cmark 会把多行源解析成**独立行**（场景 2 复现），但防御性处理仍需要。
- 单元格文本可能含 ANSI（`**bold**` 等内联样式经 `append_styled` 注入），因此按列宽折行必须**保留 ANSI 样式**，否则单元格文字样式丢失。
- 现有测试 `renders_tables_with_alignment`（render.rs:1061）断言短表格的精确输出（含 7 dash 分隔段 = 列宽 5+2），新实现必须对短表格产生字节级一致输出。
- `wrap_lines_with_breaks_wrap_expands_display_breaks`（tui/app.rs:3534）用无空格文本 "ABCDEFGHIJKL" 断言 3 行——词边界折行下无空格 token 仍硬拆为 5/5/2 → 3 行，测试保持有效。

## 3. 方案（用户已选 A）

### 3.1 render.rs — 表格渲染器重写

**新增宽度感知入口**：

```rust
pub fn markdown_to_ansi(&self, markdown: &str) -> String {
    self.markdown_to_ansi_with_width(markdown, None)
}
pub fn markdown_to_ansi_with_width(&self, markdown: &str, max_width: Option<usize>) -> String
```

- `RenderState` 增加 `table_max_width: Option<usize>` 字段（Default=None），`render_event` 的 `TagEnd::Table` 分支把 `state.table_max_width` 传入 `render_table`。
- 有效宽度解析（放在 render.rs 私有函数）：

```rust
fn resolve_table_max_width(requested: Option<usize>) -> usize {
    match requested {
        Some(w) if w > 0 => w,
        _ => crossterm::terminal::size().ok().map(|(w, _)| w as usize)
                .unwrap_or(DEFAULT_TABLE_MAX_WIDTH), // 100
    }
}
```

非 TUI 路径（stdout 为终端）→ 终端实际宽度；被重定向/测试环境 → 100 兜底。

**`render_table(&self, table: &TableState, max_width: Option<usize>)` 重写**：

1. 收集 rows（headers 优先），空表直接返回空串。
2. 每列自然宽度 = 该列所有单元格（先按 `\n` 分段再取各段 max）的 `visible_width` 最大值。
3. **比例收缩**：`table_width = Σ(width_i) + 3n + 1`（每列内容 + 左右 padding 2 + 列间 `┼` 1 + 两端 `│` 2）。若 > max_width：`available = max_width - 3n - 1`，按 `w_i * available / Σw_i` 收缩，下限 `MIN_COLUMN_WIDTH = 8`（显示列宽）；收缩后仍超 → 接受溢出（TUI 裁剪兜底）。
4. **单元格折行** `wrap_styled_text(input, col_width) -> Vec<String>`：
   - 解析 ANSI SGR：以 (前缀转义串, 字符) 为单位构建序列，维护当前激活属性栈（`0m` 重置）；每个字符记录"还原到该状态所需前缀"。
   - 先把文本按 `\n` 分段，再对每段做**词边界折行**（空格处优先断，CJK 天然按字符断；单个 token 超过列宽才硬拆）。
   - 折行点只落在字符边界，绝不落在 ANSI 转义序列中间；每行开头重发前缀。
   - 返回 `Vec<String>`（每条为 ≤ 列宽、样式保留的显示行）。
5. **多行行渲染**：每行高度 = 该行各列折行后行数的最大值；第 k 条物理显示线 = `│` + 逐列 `(cell_lines[k] 或 空串)` 按列宽补齐 + 1 空格 + `│`。头部行每条物理线都套 `bold + heading` 样式（与现状一致）。
6. 分隔行样式不变：`│─*(w0+2)┼─*(w1+2)…│`，仅在 header 与 body 之间。

**对短表格的兼容性**：自然宽度 = 现状宽度，无收缩 → 输出与现状字节一致（含分隔行、padding、header 样式），`renders_tables_with_alignment` 保持通过。

### 3.2 tui/app.rs — 词边界感知折行 + 表格行保护

`wrap_line_to_display_lines` 重写（保留样式、零宽字符处理、`area_width==0` 早退、total_width≤width 早退）：

1. **表格行保护**：首 grapheme == `│` → 整行原样返回（不折行）。超宽时由 ratatui Paragraph 右缘裁剪（对齐保持，resize 宽度不匹配时兜底）。置于宽度检查之前或之后均可，结果相同。
2. **词边界折行**：把 graphemes 切分为 token 序列（非空白=word、空白=ws）：
   - 行首：跳过前导空白；word 放不下（>area_width）时硬拆。
   - 行中：word 前若 `current_width + word_width > area_width` → 先 flush 当前行（行尾空白丢弃，不产生尾随空格）；否则追加。
   - ws token 只在放得下时追加，行尾的多余空白丢弃。
   - 每个 token 仍按现有 span 合并逻辑输出（样式保留）。
3. 纯空格行的 wrap 结果保持原行为（无词可分 → 照旧）。

**宽度传递（进程级）**：

```rust
// tui/app.rs 模块级
static OUTPUT_CONTENT_WIDTH: AtomicUsize = AtomicUsize::new(0);
```

- draw 循环（~L1014）：`OUTPUT_CONTENT_WIDTH.store(content_width, Ordering::Relaxed)`。
- emitter（~L2903）：push 前 `markdown_state.set_max_width(Some(OUTPUT_CONTENT_WIDTH.load(Ordering::Relaxed)))`。

**`MarkdownStreamState` 增加宽度字段**（render.rs:628）：

```rust
pub struct MarkdownStreamState { pending: String, max_width: Option<usize> }
// 保持 push/flush 签名不变 → streaming.rs 与现有测试零改动
pub fn with_max_width(max_width: Option<usize>) -> Self
pub fn set_max_width(&mut self, max_width: Option<usize>)
```

`push`/`flush` 内部改调 `markdown_to_ansi_with_width(&ready, self.max_width)`。

### 3.3 明确不做

- `wrap_plain_text`（输入框折行）不动——光标位置计算依赖字符级精确折行。
- ToolCard 的 `highlight_code` 语法高亮不动（工具结果以代码卡片呈现，非 markdown 表格）。
- 代码块行不折行/横向滚动——不在本次范围。

## 4. 数据流

```
draw 循环(main_area.width) ──store──> OUTPUT_CONTENT_WIDTH(AtomicUsize)
                                              │ load (每次 TextDelta/MessageStop)
emitter ──set_max_width──> MarkdownStreamState.max_width
                                    │
                           push/flush ──markdown_to_ansi_with_width(md, Some(w))──>
                           RenderState.table_max_width ──> render_table ──>
                           列宽收缩 + wrap_styled_text 折行 ──> ANSI 表格（≤ 内容区宽）
                                    │
                           ansi_to_tui → Lines → wrap_line_to_display_lines
                           （表格行原样返回；普通行词边界折行）──> 渲染
```

## 5. 测试计划

**render.rs（新增）**：
1. 长 URL 单元格 → 表格总宽 ≤ max_width，每行边框位置一致（逐行断言 `│` 起始与对齐）。
2. 单 token 超列宽 → 硬拆为多行且不断在字符中间（逐行 ≤ 列宽）。
3. CJK 单元格 → 按显示宽度折行，边框对齐。
4. 多列 + 窄宽度 → 比例收缩生效，每列 ≥ MIN_COLUMN_WIDTH。
5. 回归：`renders_tables_with_alignment` 字节级一致。

**tui/app.rs（新增）**：
6. "aa bb cc dd" width 5 → 词边界折行（`["aa bb","cc","dd"]` 或等价语义）。
7. 长 token 硬拆（无空格 12 字符 width 5 → 3 行）——与现有 `wrap_lines_with_breaks_wrap_expands_display_breaks` 一致。
8. `│` 开头行超宽 → 原样返回（不折行）。
9. 带样式的 span 折行后样式保留（fg/bg 不丢）。

**全量**：`cargo test -p rusty-claude-cli`、`cargo clippy --workspace --all-targets -- -D warnings`。

## 6. 实现可行性评审（逐项推演）

1. **签名兼容性**：`markdown_to_ansi_with_width` 为新方法，`markdown_to_ansi` 保留签名；`MarkdownStreamState::push/flush` 签名不变（仅加字段与 setter）→ streaming.rs、现有测试零改动 ✅
2. **参数来源**：`max_width` 三来源——TUI 原子值（实时内容区宽度）/ 非 TUI `None` → 查 crossterm 终端宽度 / 都失败 → 100 ✅
3. **数据传递链**：draw → 原子 → emitter setter → push → render → RenderState → render_table，逐层显式传递，无丢失 ✅
4. **判定优先级**：表格行保护（`│` 前缀）> 词边界折行 > 硬拆。表格行绝不词折行 ✅
5. **retry/重入**：`markdown_to_ansi` 每个流式片段被调用一次；wrap 缓存（内容指针+宽度）逻辑不变；原子读为 Relaxed load，开销可忽略 ✅
6. **冲突处理**：resize 使已渲染表格超宽 → 表格行不折行改右缘裁剪，左对齐保持，避免 mid-cell 断行 ✅
7. **与现有系统重叠**：`wrap_plain_text`、`highlight_code`、sticky/scroll 逻辑均不受影响；display_breaks 语义（按 entry 分组）不变 ✅
8. **失败路径**：`crossterm::terminal::size()` 在非终端（测试/重定向）返回 Err → 100 兜底；`wrap_styled_text` 对畸形 ANSI 防御（前缀解析失败则按纯文本折行）；`ansi_to_tui` 失败沿用现有 fallback ✅
9. **成本估算**：render.rs 新增 ~120 行（wrap_styled_text ~60 + render_table 重写 ~50 + 宽度解析 ~10）+ 测试；tui/app.rs 重写 ~40 行 + 原子接线 ~10 行 + 测试；MarkdownStreamState 改动 ~8 行。总计约 250 行（含测试）✅

## 7. 风险与缓解

| 风险 | 缓解 |
| --- | --- |
| 表格渲染改动破坏现有精确输出断言 | 短表格路径保持字节一致，回归测试兜底 |
| ANSI 样式在折行中丢失/损坏 | `wrap_styled_text` 基于字符级前缀还原，断点只落在字符边界 |
| resize 与缓存内容宽度不匹配 | 表格行裁剪兜底 + 原子宽度实时更新 |
| 词边界折行改变 display 行数影响滚动 | display_breaks 按 entry 重新统计，语义不变 |
