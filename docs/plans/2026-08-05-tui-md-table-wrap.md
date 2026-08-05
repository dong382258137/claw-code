# TUI Markdown 表格对齐 + 换行可读性 实现计划

**Goal:** 修复 TUI 输出中 MD 表格错乱（长内容不按列折行）与普通文本折行拆词问题，使表格永远整体对齐、英文单词/URL 不被拆断。

**Architecture:** render.rs 的 `render_table` 重写为宽度感知（列宽比例收缩 + 单元格按列宽 ANSI 感知词边界折行）；`MarkdownStreamState` 增加 `max_width` 字段并新增 `markdown_to_ansi_with_width` 入口；TUI 侧 `wrap_line_to_display_lines` 改为词边界感知折行，表格行（`│` 开头）不折行由右缘裁剪兜底；draw 循环把内容区宽度写入进程级 `AtomicUsize` 供 emitter 读取。

**Tech Stack:** Rust, pulldown-cmark 0.13, ratatui 0.29, ansi-to-tui 7, unicode-width 0.2, crossterm 0.28

---

## 现状分析（代码事实核查）

| 组件 | 位置 | 现状 | 验证 |
| --- | --- | --- | --- |
| `render_markdown` / `markdown_to_ansi` | render.rs:272-290, 295-297 | 无宽度概念 | ✅ |
| `RenderState` | render.rs:152-163 | 无宽度字段，`#[derive(Default)]` | ✅ |
| `render_event` Table 分支 | render.rs:407-444 | `TagEnd::Table` → `self.render_table(&table)` | ✅ |
| `render_table` | render.rs:518-566 | 列宽无上限、单元格不折行 | ✅ |
| `render_table_row` | render.rs:568-585 | 单行直拼，`\n` 会断行 | ✅ |
| `visible_width` / `strip_ansi` | render.rs:923-948 | unicode-width + 剥 ANSI | ✅ |
| `find_stream_safe_boundary` | render.rs:852-883 | 表格在空行/闭合 fence 时才整表 flush（表格天然整块渲染） | ✅ |
| `MarkdownStreamState` | render.rs:628-652 | `push/flush` 签名固定，无宽度 | ✅ |
| `wrap_line_to_display_lines` | tui/app.rs:310-397 | grapheme 边界折行、无词边界感知 | ✅ |
| `wrap_lines_with_breaks` | tui/app.rs:446-475 | 按 entry 分组 wrap + display breaks | ✅ |
| draw 循环 wrap | tui/app.rs:1014-1021 | `content_width = main_area.width` | ✅ |
| emitter TextDelta push | tui/app.rs:2899-2908 | `ms.push(renderer, &text)` | ✅ |
| emitter MessageStop flush | tui/app.rs:3026-3035 | `ms.flush(renderer)` | ✅ |
| app.rs 顶部导入 | tui/app.rs:11-21 | `use std::sync::{Arc, Mutex}`，无 atomic | ✅ |
| streaming.rs 非 TUI 流式 | streaming.rs:513, 648 | `MarkdownStreamState::default()` + `push` | ✅（零改动） |
| `renders_tables_with_alignment` 回归 | render.rs:1061-1071 | 断言短表格精确输出（列宽 5/5） | ✅ 必须保持字节一致 |
| `wrap_lines_with_breaks_wrap_expands_display_breaks` | tui/app.rs:3534-3545 | "ABCDEFGHIJKL" width 5 → 3 行（无空格 token 硬拆，词边界折行下仍成立） | ✅ |
| `wrap_plain_text` | tui/app.rs:409-437 | 输入框折行，**不修改** | ✅ |
| ToolCard 高亮 | tui/tool_card.rs:136-146 | `highlight_code`，**不修改** | ✅ |

---

## 文件结构

- 修改：`rust/crates/rusty-claude-cli/src/render.rs`（表格渲染 + 流式宽度）
- 修改：`rust/crates/rusty-claude-cli/src/tui/app.rs`（词边界折行 + 宽度原子）
- 删除：`rust/crates/rusty-claude-cli/examples/repro_md.rs`（临时复现脚本）
- 测试：均内联在对应文件的 `mod tests` 中（沿用现有风格）

---

### Task 1: render.rs — ANSI 感知单元格折行辅助函数

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/render.rs`（新增常量与函数，位于 `visible_width` 附近 ~L948）

- [ ] **Step 1: 写失败测试**（追加到 render.rs `mod tests`，先更新该模块顶部导入）

```rust
use super::{strip_ansi, visible_width, wrap_cell_lines, MarkdownStreamState, Spinner, TerminalRenderer};
```

```rust
#[test]
fn wraps_cell_with_ansi_styles_at_word_boundaries() {
    // 带 \x1b[1m 粗体样式的单元格,宽度 6 词边界折行
    let styled = "\u{1b}[1mhello world\u{1b}[0m";
    let lines = wrap_cell_lines(styled, 6);
    assert_eq!(lines.len(), 2, "应折成 2 行");
    assert_eq!(strip_ansi(&lines[0]), "hello");
    assert_eq!(strip_ansi(&lines[1]), "world");
    // 样式保留:每行都应包含粗体序列
    assert!(lines[0].contains("\u{1b}[1m"));
    assert!(lines[1].contains("\u{1b}[1m"));
    // 每行可见宽度 ≤ 6
    for line in &lines {
        assert!(visible_width(line) <= 6, "行超宽: {line:?}");
    }
}

#[test]
fn wraps_cell_splits_overwide_single_token() {
    // 无空格超宽 token:硬拆为 5/5/2
    let lines = wrap_cell_lines("ABCDEFGHIJKL", 5);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["ABCDE", "FGHIJ", "KL"]);
}

#[test]
fn wraps_cell_breaks_on_newlines() {
    // 换行强制分段
    let lines = wrap_cell_lines("aaa\nbbb ccc", 6);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(plain, vec!["aaa", "bbb", "ccc"]);
}

#[test]
fn wraps_cell_handles_plain_text_and_cjk() {
    let lines = wrap_cell_lines("这是很长的一段中文文本", 8);
    for line in &lines {
        assert!(visible_width(line) <= 8, "CJK 行超宽: {line:?}");
    }
    assert!(lines.len() >= 2, "8 列宽应容纳不下整句");
    let joined: String = lines.iter().map(|l| strip_ansi(l)).collect();
    assert_eq!(joined, "这是很长的一段中文文本", "折行不应丢字符");
}
```

- [ ] **Step 2: 运行确认失败**

```
cd rust
cargo test -p rusty-claude-cli wraps_cell 2>&1 | tail -20
```

Expected: `error[E0425]: cannot find function wrap_cell_lines in this scope`

- [ ] **Step 3: 实现辅助函数**（追加到 `visible_width` 之后，即 `strip_ansi` 函数后）

```rust
/// 表格渲染默认最大总宽度（终端尺寸查询失败时的兜底）。
const DEFAULT_TABLE_MAX_WIDTH: usize = 100;
/// 列宽收缩时单列最小显示宽度。
const MIN_TABLE_COLUMN_WIDTH: usize = 8;

/// 解析表格渲染目标宽度：显式指定 > 0 优先，否则查终端宽度，最后兜底默认值。
fn resolve_table_max_width(requested: Option<usize>) -> usize {
    if let Some(width) = requested {
        if width > 0 {
            return width;
        }
    }
    crossterm::terminal::size()
        .ok()
        .and_then(|(width, _)| (width > 0).then_some(width as usize))
        .unwrap_or(DEFAULT_TABLE_MAX_WIDTH)
}

/// ANSI 字符串解析单元：字符 + 从干净状态渲染它所需的前缀 + 该字符是否带样式。
struct AnsiUnit {
    /// 渲染该字符前必须输出的转义前缀（`\x1b[0m` + 激活的 SGR 序列）。
    prefix: String,
    ch: char,
    /// 该字符是否处于非默认样式（用于行尾是否需要补 reset）。
    styled: bool,
}

/// 解析 ANSI SGR 字符串为 (前缀, 字符) 单元序列。
///
/// 前缀规则（保证任意断点处新行从干净状态正确渲染）：
/// - 带样式字符：`\x1b[0m` + 当前激活的全部 SGR 序列（自愈式重建）；
/// - 紧跟在带样式字符后的无样式字符：`\x1b[0m`（清除泄漏的样式）；
/// - 其余：空字符串。
/// 非 SGR 转义（OSC 等）作为零宽透传，追加到后续字符前缀。
fn parse_ansi_units(input: &str) -> Vec<AnsiUnit> {
    let mut units: Vec<AnsiUnit> = Vec::new();
    let mut active: Vec<String> = Vec::new();
    let mut prev_styled = false;
    let mut pending_escape = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            let mut seq = String::from("\u{1b}");
            if chars.peek() == Some(&'[') {
                chars.next();
                seq.push('[');
                for next in chars.by_ref() {
                    seq.push(next);
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                if seq.ends_with('m') {
                    let params = &seq[2..seq.len() - 1];
                    if params == "0" {
                        active.clear();
                    } else {
                        active.push(seq.clone());
                    }
                } else {
                    // 非 SGR（如 cursor 移动）:按零宽透传
                    pending_escape.push_str(&seq);
                    continue;
                }
            } else if chars.peek() == Some(&']') {
                chars.next();
                seq.push(']');
                for next in chars.by_ref() {
                    seq.push(next);
                    if next == '\u{07}' {
                        break;
                    }
                }
                pending_escape.push_str(&seq);
                continue;
            } else if let Some(&next) = chars.peek() {
                seq.push(next);
                chars.next();
                pending_escape.push_str(&seq);
                continue;
            }
            // SGR 更新 active 后不产出字符单元
            continue;
        }
        let styled = !active.is_empty();
        let prefix = if styled {
            format!("\u{1b}[0m{}", active.concat())
        } else if prev_styled {
            String::from("\u{1b}[0m")
        } else {
            String::new()
        };
        let prefix = pending_escape + &prefix;
        pending_escape.clear();
        units.push(AnsiUnit { prefix, ch, styled });
        prev_styled = styled;
    }
    units
}

/// 单元格文本（可能含 ANSI 样式与 `\n`）按指定显示宽度折行，返回样式保留的显示行。
///
/// - `\n` 强制分段；空格处优先断行；单个 token 超过列宽才硬拆；
/// - CJK/emoji 按显示宽度计数；
/// - `width == 0` 时仅按 `\n` 分段，不折行。
fn wrap_cell_lines(cell: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return cell.split('\n').map(str::to_string).collect();
    }
    let units = parse_ansi_units(cell);
    let mut segments: Vec<Vec<AnsiUnit>> = vec![Vec::new()];
    for unit in units {
        if unit.ch == '\n' {
            segments.push(Vec::new());
        } else if let Some(last) = segments.last_mut() {
            last.push(unit);
        }
    }
    let mut lines = Vec::new();
    for segment in &segments {
        wrap_segment_lines(segment, width, &mut lines);
    }
    lines
}

fn char_display_width(ch: char) -> usize {
    unicode_width::UnicodeWidthStr::width(&ch.to_string())
}

/// 把当前行 flush 到输出，行尾若残留样式则补 `\x1b[0m`（保证续行从干净状态开始）。
fn flush_ansi_line(
    current: &mut String,
    current_width: &mut usize,
    last_styled: &mut bool,
    out: &mut Vec<String>,
) {
    if *last_styled {
        current.push_str("\u{1b}[0m");
    }
    if !current.is_empty() {
        out.push(std::mem::take(current));
    }
    *current_width = 0;
    *last_styled = false;
}

fn emit_ansi_units(
    units: &[&AnsiUnit],
    target: &mut String,
    width: &mut usize,
    last_styled: &mut bool,
) {
    for unit in units {
        if !unit.prefix.is_empty() {
            target.push_str(&unit.prefix);
        }
        target.push(unit.ch);
        *width += char_display_width(unit.ch);
        *last_styled = unit.styled;
    }
}

/// 词边界折行一个文本段（已按 `\n` 分段）。
fn wrap_segment_lines(segment: &[AnsiUnit], width: usize, out: &mut Vec<String>) {
    if segment.is_empty() {
        out.push(String::new());
        return;
    }
    // tokenize:word(非空白) / ws(空白) 交替
    struct Token<'a> {
        units: Vec<&'a AnsiUnit>,
        is_ws: bool,
    }
    let mut tokens: Vec<Token> = Vec::new();
    for unit in segment {
        let is_ws = unit.ch.is_whitespace();
        if let Some(last) = tokens.last_mut() {
            if last.is_ws == is_ws {
                last.units.push(unit);
                continue;
            }
        }
        tokens.push(Token {
            units: vec![unit],
            is_ws,
        });
    }

    let mut current = String::new();
    let mut current_width = 0usize;
    let mut last_styled = false;
    let mut pending_ws: Vec<&AnsiUnit> = Vec::new();

    for token in &tokens {
        if token.is_ws {
            pending_ws.extend(token.units.iter().copied());
            continue;
        }
        let word_width: usize = token
            .units
            .iter()
            .map(|u| char_display_width(u.ch))
            .sum();
        let ws_width: usize = pending_ws.iter().map(|u| char_display_width(u.ch)).sum();
        if current_width + ws_width + word_width <= width {
            emit_ansi_units(
                &pending_ws,
                &mut current,
                &mut current_width,
                &mut last_styled,
            );
            pending_ws.clear();
            emit_ansi_units(
                &token.units,
                &mut current,
                &mut current_width,
                &mut last_styled,
            );
        } else if word_width <= width {
            flush_ansi_line(
                &mut current,
                &mut current_width,
                &mut last_styled,
                out,
            );
            pending_ws.clear();
            emit_ansi_units(
                &token.units,
                &mut current,
                &mut current_width,
                &mut last_styled,
            );
        } else {
            // 单词本身超宽:flush 当前行后硬拆
            flush_ansi_line(&mut current, &mut current_width, &mut last_styled, out);
            pending_ws.clear();
            for unit in &token.units {
                let w = char_display_width(unit.ch);
                if w == 0 {
                    if !unit.prefix.is_empty() {
                        current.push_str(&unit.prefix);
                    }
                    current.push(unit.ch);
                    last_styled = unit.styled;
                    continue;
                }
                if current_width + w > width && current_width > 0 {
                    flush_ansi_line(&mut current, &mut current_width, &mut last_styled, out);
                }
                if !unit.prefix.is_empty() {
                    current.push_str(&unit.prefix);
                }
                current.push(unit.ch);
                current_width += w;
                last_styled = unit.styled;
            }
        }
    }
    flush_ansi_line(&mut current, &mut current_width, &mut last_styled, out);
}
```

- [ ] **Step 4: 运行确认通过**

```
cargo test -p rusty-claude-cli wraps_cell 2>&1 | tail -20
```

Expected: 4 个 `wraps_cell_*` 测试全部 PASS

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/render.rs
git commit -m "feat(render): ANSI-aware cell wrapping helpers for tables"
```

---

### Task 2: render.rs — 宽度感知表格渲染重写

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/render.rs`（`RenderState` L152-163、`render_markdown` L272-290、`markdown_to_ansi` L295-297、`render_event` L407-412、`render_table` L518-566、`render_table_row` L568-585）

- [ ] **Step 1: 写失败测试**（追加到 render.rs `mod tests`）

```rust
#[test]
fn renders_tables_wrap_long_cells_and_stay_aligned() {
    let terminal_renderer = TerminalRenderer::new();
    let md = "\
| 工具 | 说明 |
| ---- | ---- |
| read_file | 读取 https://raw.githubusercontent.com/example/very/long/path/to/a/documentation/file.md |
| write_file | 写入文件 |
";
    // 目标宽度 40:列收缩 + 单元格折行
    let markdown_output = terminal_renderer.markdown_to_ansi_with_width(md, Some(40));
    let plain_text = strip_ansi(&markdown_output);
    let lines = plain_text.lines().collect::<Vec<_>>();
    assert!(lines.len() >= 4, "长表格应折成多行: {}", lines.len());
    let widths: std::collections::BTreeSet<usize> =
        lines.iter().map(|l| visible_width(l)).collect();
    assert_eq!(widths.len(), 1, "所有表格行宽度应一致(对齐): {widths:?}");
    for line in &lines {
        assert!(line.starts_with('│'), "应以边框开头: {line:?}");
        assert!(line.ends_with('│'), "应以边框结尾: {line:?}");
        assert!(visible_width(line) <= 40, "不应超过目标宽度: {line:?}");
    }
}

#[test]
fn renders_tables_shrink_columns_proportionally() {
    let terminal_renderer = TerminalRenderer::new();
    let long = "x".repeat(50);
    let md = format!("| a | b |\n| - | - |\n| {long} | 1 |");
    let markdown_output = terminal_renderer.markdown_to_ansi_with_width(&md, Some(40));
    let plain_text = strip_ansi(&markdown_output);
    for line in plain_text.lines() {
        assert!(visible_width(line) <= 40, "收缩后仍超宽: {line:?}");
        assert!(line.starts_with('│') && line.ends_with('│'));
    }
}

#[test]
fn renders_tables_cjk_cells_stay_aligned() {
    let terminal_renderer = TerminalRenderer::new();
    let md = "| 名称 | 数量 |\n| ---- | ---- |\n| 苹果 | 10 |\n| 香蕉香蕉香蕉 | 3 |";
    let markdown_output = terminal_renderer.markdown_to_ansi_with_width(md, Some(24));
    let plain_text = strip_ansi(&markdown_output);
    let widths: std::collections::BTreeSet<usize> = plain_text
        .lines()
        .map(|l| visible_width(l))
        .collect();
    assert_eq!(widths.len(), 1, "CJK 表格行应对齐: {widths:?}");
}
```

- [ ] **Step 2: 运行确认失败**

```
cargo test -p rusty-claude-cli renders_tables 2>&1 | tail -20
```

Expected: `markdown_to_ansi_with_width` 不存在（E0599/编译错误）或长表格测试断言失败

- [ ] **Step 3: 实现**

3a. `RenderState` 增加字段（L152-163 区域）：

```rust
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RenderState {
    emphasis: usize,
    strong: usize,
    heading_level: Option<u8>,
    quote: usize,
    list_stack: Vec<ListKind>,
    link_stack: Vec<LinkState>,
    table: Option<TableState>,
    /// 表格渲染目标最大宽度（None 时按终端宽度/默认值解析）。
    table_max_width: Option<usize>,
}
```

3b. `render_markdown` 拆出宽度变体（L272-290）：

```rust
#[must_use]
pub fn render_markdown(&self, markdown: &str) -> String {
    self.render_markdown_with_width(markdown, None)
}

fn render_markdown_with_width(&self, markdown: &str, max_width: Option<usize>) -> String {
    let normalized = normalize_nested_fences(markdown);
    let mut output = String::new();
    let mut state = RenderState::default();
    state.table_max_width = max_width;
    let mut code_language = String::new();
    let mut code_buffer = String::new();
    let mut in_code_block = false;

    for event in Parser::new_ext(&normalized, Options::all()) {
        self.render_event(
            event,
            &mut state,
            &mut output,
            &mut code_buffer,
            &mut code_language,
            &mut in_code_block,
        );
    }

    output.trim_end().to_string()
}
```

3c. `markdown_to_ansi` 宽度变体（L295-297 区域）：

```rust
#[must_use]
pub fn markdown_to_ansi(&self, markdown: &str) -> String {
    self.render_markdown_with_width(markdown, None)
}

/// 带目标宽度的 markdown → ANSI 渲染。`max_width` 为 Some(>0) 时作为
/// 表格宽度上限；None 或 0 时按终端宽度（查询失败兜底 100）。
#[must_use]
pub fn markdown_to_ansi_with_width(&self, markdown: &str, max_width: Option<usize>) -> String {
    self.render_markdown_with_width(markdown, max_width)
}
```

3d. `render_event` Table 结束分支（L408-412）：

```rust
Event::End(TagEnd::Table) => {
    if let Some(table) = state.table.take() {
        output.push_str(&self.render_table(&table, state.table_max_width));
        output.push_str("\n\n");
    }
}
```

3e. 用 `fit_column_widths` 重写 `render_table`（替换 L518-566 整体）：

```rust
/// 按目标总宽收缩列宽（比例收缩，下限 MIN_TABLE_COLUMN_WIDTH）。
/// 返回每列最终显示宽度。
fn fit_column_widths(natural: &[usize], target_width: usize) -> Vec<usize> {
    let n = natural.len();
    if n == 0 {
        return Vec::new();
    }
    // 表格总宽 = Σ(w_i) + 3n + 1：每列内容 + 左右 padding 2、列间 ┼ 1、两端 │ 2
    let base = 3 * n + 1;
    let total: usize = natural.iter().sum::<usize>() + base;
    if total <= target_width {
        return natural.to_vec();
    }
    let available = target_width.saturating_sub(base);
    let sum_natural: usize = natural.iter().sum::<usize>().max(1);
    let mut widths: Vec<usize> = natural
        .iter()
        .map(|w| (w.saturating_mul(available) / sum_natural).max(MIN_TABLE_COLUMN_WIDTH))
        .collect();
    // 迭代收缩直到总和 ≤ available（每列下限 MIN_TABLE_COLUMN_WIDTH）
    loop {
        let current_total: usize = widths.iter().sum();
        if current_total <= available {
            break;
        }
        let excess = current_total - available;
        let capacity_sum: usize = widths
            .iter()
            .map(|w| w.saturating_sub(MIN_TABLE_COLUMN_WIDTH))
            .sum();
        if capacity_sum == 0 {
            break; // 全部已到下限，接受溢出（TUI 裁剪兜底）
        }
        let mut remaining = excess.min(capacity_sum);
        let mut changed = false;
        for w in widths.iter_mut() {
            if remaining == 0 {
                break;
            }
            let capacity = w.saturating_sub(MIN_TABLE_COLUMN_WIDTH);
            if capacity > 0 {
                let take = capacity.min(remaining);
                *w -= take;
                remaining -= take;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    widths
}

/// 单元格按 `\n` 分段后各段的可见宽度最大值（列宽计算用）。
fn max_visible_line_width(cell: &str) -> usize {
    cell.split('\n')
        .map(visible_width)
        .max()
        .unwrap_or(0)
}

fn render_table(&self, table: &TableState, max_width: Option<usize>) -> String {
    let mut rows = Vec::new();
    if !table.headers.is_empty() {
        rows.push(table.headers.clone());
    }
    rows.extend(table.rows.iter().cloned());

    if rows.is_empty() {
        return String::new();
    }

    let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let target_width = resolve_table_max_width(max_width);

    let natural_widths: Vec<usize> = (0..column_count)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| max_visible_line_width(cell))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let widths = fit_column_widths(&natural_widths, target_width);

    let border = format!("{}", "│".with(self.color_theme.table_border));
    let separator = widths
        .iter()
        .map(|width| "─".repeat(*width + 2))
        .collect::<Vec<_>>()
        .join(&format!("{}", "┼".with(self.color_theme.table_border)));
    let separator = format!("{border}{separator}{border}");

    let mut output = String::new();
    if !table.headers.is_empty() {
        output.push_str(&self.render_table_row(&table.headers, &widths, true));
        output.push('\n');
        output.push_str(&separator);
        if !table.rows.is_empty() {
            output.push('\n');
        }
    }

    for (index, row) in table.rows.iter().enumerate() {
        output.push_str(&self.render_table_row(row, &widths, false));
        if index + 1 < table.rows.len() {
            output.push('\n');
        }
    }

    output
}
```

3f. 重写 `render_table_row`（替换 L568-585 整体）：

```rust
/// 渲染一行（支持单元格按列宽折行成多物理行，所有物理行边框对齐）。
fn render_table_row(&self, row: &[String], widths: &[usize], is_header: bool) -> String {
    let border = format!("{}", "│".with(self.color_theme.table_border));
    // 每列折行（含 `\n` 分段与 ANSI 样式保留）
    let cell_lines: Vec<Vec<String>> = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let cell = row.get(index).map_or("", String::as_str);
            wrap_cell_lines(cell, *width)
        })
        .collect();
    let height = cell_lines.iter().map(Vec::len).max().unwrap_or(1).max(1);

    let mut output = String::new();
    for line_index in 0..height {
        let mut line = border.clone();
        for (index, width) in widths.iter().enumerate() {
            let cell_line = cell_lines
                .get(index)
                .and_then(|lines| lines.get(line_index))
                .map_or("", String::as_str);
            line.push(' ');
            if is_header {
                let _ = write!(line, "{}", cell_line.bold().with(self.color_theme.heading));
            } else {
                line.push_str(cell_line);
            }
            let padding = width.saturating_sub(visible_width(cell_line));
            line.push_str(&" ".repeat(padding + 1));
            line.push_str(&border);
        }
        output.push_str(&line);
        if line_index + 1 < height {
            output.push('\n');
        }
    }
    output
}
```

（`resolve_table_max_width` 使用全路径 `crossterm::terminal::size()` 调用，无需修改 render.rs 顶部导入。）

- [ ] **Step 4: 运行确认通过（含回归）**

```
cargo test -p rusty-claude-cli renders_tables 2>&1 | tail -30
```

Expected: 新增 3 个测试 + 现有 `renders_tables_with_alignment` 全部 PASS

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/render.rs
git commit -m "fix(render): width-capped table rendering with per-column wrapping"
```

---

### Task 3: render.rs — MarkdownStreamState 宽度传播

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/render.rs`（`MarkdownStreamState` L628-652）

- [ ] **Step 1: 写失败测试**（追加到 render.rs `mod tests`）

```rust
#[test]
fn streaming_state_applies_max_width_to_tables() {
    let renderer = TerminalRenderer::new();
    let long = "x".repeat(50);
    let md = format!("| a | b |\n| - | - |\n| {long} | 1 |\n\n");
    let mut state = MarkdownStreamState::with_max_width(Some(40));
    let flushed = state
        .push(&renderer, &md)
        .expect("blank line flushes");
    for line in strip_ansi(&flushed).lines() {
        assert!(visible_width(line) <= 40, "流式渲染未应用宽度: {line:?}");
    }
    // 无宽度（默认）时按终端宽度/100 解析，不应 panic
    let mut default_state = MarkdownStreamState::default();
    assert!(default_state.push(&renderer, &md).is_some());
}
```

- [ ] **Step 2: 运行确认失败**

```
cargo test -p rusty-claude-cli streaming_state_applies_max_width 2>&1 | tail -15
```

Expected: `no function or associated item named with_max_width found`

- [ ] **Step 3: 实现**（替换 L628-652 的 `MarkdownStreamState` 定义与 `impl`）

```rust
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MarkdownStreamState {
    pending: String,
    /// 表格渲染目标宽度（None → 终端宽度/默认值）。
    max_width: Option<usize>,
}

impl MarkdownStreamState {
    /// 携带初始目标宽度的构造器。
    #[must_use]
    pub fn with_max_width(max_width: Option<usize>) -> Self {
        Self {
            pending: String::new(),
            max_width,
        }
    }

    /// 更新目标宽度（TUI draw 循环每帧刷新内容区宽度后由 emitter 调用）。
    pub fn set_max_width(&mut self, max_width: Option<usize>) {
        self.max_width = max_width;
    }

    #[must_use]
    pub fn push(&mut self, renderer: &TerminalRenderer, delta: &str) -> Option<String> {
        self.pending.push_str(delta);
        let split = find_stream_safe_boundary(&self.pending)?;
        let ready = self.pending[..split].to_string();
        self.pending.drain(..split);
        Some(renderer.markdown_to_ansi_with_width(&ready, self.max_width))
    }

    #[must_use]
    pub fn flush(&mut self, renderer: &TerminalRenderer) -> Option<String> {
        if self.pending.trim().is_empty() {
            self.pending.clear();
            None
        } else {
            let pending = std::mem::take(&mut self.pending);
            Some(renderer.markdown_to_ansi_with_width(&pending, self.max_width))
        }
    }
}
```

注意：`set_max_width` 在流式中间调用只影响后续渲染（已渲染片段不回改）——符合预期。

- [ ] **Step 4: 运行确认通过**

```
cargo test -p rusty-claude-cli streaming_state 2>&1 | tail -25
```

Expected: 新增测试 + 现有 4 个 `streaming_state_*` 测试全部 PASS

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/render.rs
git commit -m "feat(render): MarkdownStreamState width propagation"
```

---

### Task 4: tui/app.rs — 词边界感知折行 + 表格行保护

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/app.rs`（`wrap_line_to_display_lines` L310-397）

- [ ] **Step 1: 写失败测试**（追加到 tui/app.rs `mod tests`，现有测试模块在 L3500 之后）

```rust
#[test]
fn wrap_words_not_broken_midword() {
    let line = Line::raw("foo bar baz qux");
    let wrapped = wrap_line_to_display_lines(&line, 7);
    let texts: Vec<String> = wrapped.iter().map(|l| l.to_string()).collect();
    assert_eq!(texts, vec!["foo bar", "baz qux"], "单词不应被拆断");
}

#[test]
fn wrap_splits_overwide_words() {
    let line = Line::raw("ABCDEFGHIJKL");
    let wrapped = wrap_line_to_display_lines(&line, 5);
    let texts: Vec<String> = wrapped.iter().map(|l| l.to_string()).collect();
    assert_eq!(texts, vec!["ABCDE", "FGHIJ", "KL"], "超宽无空格 token 应硬拆");
}

#[test]
fn wrap_leaves_table_rows_unwrapped() {
    let row = "│ aaa │ bbbbbbbbbbbbbbbbbbbb │";
    let line = Line::raw(row);
    let wrapped = wrap_line_to_display_lines(&line, 10);
    assert_eq!(wrapped.len(), 1, "表格行不应折行");
    assert_eq!(wrapped[0].to_string(), row, "表格行应原样保留");
}

#[test]
fn wrap_preserves_span_styles_across_lines() {
    let line = Line::from(vec![
        Span::styled("bold", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(" text", Style::default()),
        Span::styled("here", Style::default().fg(Color::Red)),
    ]);
    let wrapped = wrap_line_to_display_lines(&line, 9);
    assert_eq!(wrapped.len(), 2, "bold + texthere 应在 9 列下折成 2 行");
    assert!(
        wrapped[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
        "第一行应保留 bold 样式"
    );
    assert!(
        wrapped[1].spans.iter().any(|s| s.style.fg == Some(Color::Red)),
        "第二行应保留红色样式"
    );
}
```

- [ ] **Step 2: 运行确认失败**

```
cargo test -p rusty-claude-cli wrap_words_not_broken_midword 2>&1 | tail -15
```

Expected: FAIL（当前按字符折行，`"foo bar baz qux"` 在 7 列下折为 `["foo bar", " baz", " qux"]`）

- [ ] **Step 3: 实现**（整体替换 `wrap_line_to_display_lines` L310-397）

```rust
fn wrap_line_to_display_lines(line: &Line<'static>, area_width: usize) -> Vec<Line<'static>> {
    if area_width == 0 {
        return vec![line.clone()];
    }
    // 用 styled_graphemes 迭代，保留每个 grapheme 的样式。
    let graphemes: Vec<StyledGrapheme<'_>> = line.styled_graphemes(Style::default()).collect();
    let total_width: usize = graphemes
        .iter()
        .map(|g| unicode_width::UnicodeWidthStr::width(g.symbol))
        .sum();
    if total_width <= area_width {
        return vec![line.clone()];
    }
    // 表格行保护：render.rs 渲染的表格行以 │ 开头，宽度已按内容区收缩；
    // 若因 resize 宽度不匹配而超宽，直接原样返回（Paragraph 右缘裁剪），
    // 避免在单元格中间折行造成边框错位。
    if let Some(first) = graphemes.first() {
        if first.symbol == "│" {
            return vec![line.clone()];
        }
    }

    let mut result: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_span_str = String::new();
    let mut current_span_style = Style::default();
    let mut has_span = false;
    let mut current_width: usize = 0;

    // 把当前累积的 span 推入 current_spans
    macro_rules! flush_span {
        () => {
            if has_span && !current_span_str.is_empty() {
                current_spans.push(Span::styled(
                    std::mem::take(&mut current_span_str),
                    current_span_style,
                ));
                has_span = false;
            }
        };
    }
    // 把 current_spans 推入 result，开始新行
    macro_rules! flush_line {
        () => {
            flush_span!();
            if !current_spans.is_empty() {
                let new_line = Line {
                    spans: std::mem::take(&mut current_spans),
                    style: line.style,
                    alignment: line.alignment,
                };
                result.push(new_line);
            }
            current_width = 0;
        };
    }
    // 追加一个 grapheme 到当前 span（style 相同合并，不同则新建）
    macro_rules! append_grapheme {
        ($g:expr) => {
            if has_span && current_span_style == $g.style {
                current_span_str.push_str($g.symbol);
            } else {
                flush_span!();
                current_span_str = $g.symbol.to_string();
                current_span_style = $g.style;
                has_span = true;
            }
        };
    }

    // 词边界 token 化：word(非空白) / ws(空白) 交替
    struct WToken<'a> {
        graphemes: Vec<&'a StyledGrapheme<'a>>,
        is_ws: bool,
    }
    let mut tokens: Vec<WToken> = Vec::new();
    for g in &graphemes {
        let is_ws = g
            .symbol
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        if let Some(last) = tokens.last_mut() {
            if last.is_ws == is_ws {
                last.graphemes.push(g);
                continue;
            }
        }
        tokens.push(WToken {
            graphemes: vec![g],
            is_ws,
        });
    }

    let mut pending_ws: Vec<&StyledGrapheme> = Vec::new();

    for token in &tokens {
        if token.is_ws {
            pending_ws.extend(token.graphemes.iter().copied());
            continue;
        }
        let word_width: usize = token
            .graphemes
            .iter()
            .map(|g| unicode_width::UnicodeWidthStr::width(g.symbol))
            .sum();
        let ws_width: usize = pending_ws
            .iter()
            .map(|g| unicode_width::UnicodeWidthStr::width(g.symbol))
            .sum();
        if current_width + ws_width + word_width <= area_width {
            // 整词放得下：先输出暂存空白，再输出词
            for g in &pending_ws {
                append_grapheme!(g);
                current_width += unicode_width::UnicodeWidthStr::width(g.symbol);
            }
            pending_ws.clear();
            for g in &token.graphemes {
                append_grapheme!(g);
                current_width += unicode_width::UnicodeWidthStr::width(g.symbol);
            }
        } else if word_width <= area_width {
            // 词单独放得下：换行后输出（丢弃行尾暂存空白）
            flush_line!();
            pending_ws.clear();
            for g in &token.graphemes {
                append_grapheme!(g);
                current_width += unicode_width::UnicodeWidthStr::width(g.symbol);
            }
        } else {
            // 词本身超宽：硬拆（不拆转义/样式，按 grapheme 边界）
            flush_line!();
            pending_ws.clear();
            for g in &token.graphemes {
                let gw = unicode_width::UnicodeWidthStr::width(g.symbol);
                if gw == 0 {
                    // 零宽字符：追加到当前 span（不触发换行）
                    append_grapheme!(g);
                    continue;
                }
                if current_width + gw > area_width && current_width > 0 {
                    flush_line!();
                }
                append_grapheme!(g);
                current_width += gw;
            }
        }
    }
    // flush 最后一行
    flush_line!();

    if result.is_empty() {
        // 安全兜底：不应触发（total_width > area_width 保证至少一行）
        vec![line.clone()]
    } else {
        result
    }
}
```

注意：零宽分支中 `append_grapheme!` 后未累加 `current_width`（宽度 0），与 `current_width += gw` 语义一致。

- [ ] **Step 4: 运行确认通过（含回归）**

```
cargo test -p rusty-claude-cli wrap_ 2>&1 | tail -30
```

Expected: 新增 4 个测试 + 现有 `wrap_lines_with_breaks_*` 全部 PASS（"ABCDEFGHIJKL" 无空格 → 硬拆 5/5/2 = 3 行不变）

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/tui/app.rs
git commit -m "fix(tui): word-aware line wrapping + table-row clip protection"
```

---

### Task 5: tui/app.rs — 内容区宽度写入原子并接入 emitter

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/app.rs`（顶部导入 L11-21、draw 循环 L1014、emitter L2903、MessageStop L3030）

- [ ] **Step 1: 写失败测试**（追加到 tui/app.rs `mod tests`）

> 注：`build_test_emitter`（L3440）是简化版 emitter，TextDelta 直接 `append(&text)`，
> 不走 markdown_state 渲染路径，因此无法通过它断言"宽度传入表格"（该链路由编译期
> 类型检查保证 + Task 3 的 `streaming_state_applies_max_width_to_tables` 覆盖渲染行为）。
> 此测试只验证原子静态存在且可读写，并验证 emitter 在宽度接线后仍正常工作。

```rust
#[test]
fn output_content_width_static_stores_and_loads() {
    use std::sync::atomic::Ordering;
    OUTPUT_CONTENT_WIDTH.store(40, Ordering::Relaxed);
    assert_eq!(OUTPUT_CONTENT_WIDTH.load(Ordering::Relaxed), 40);
    OUTPUT_CONTENT_WIDTH.store(0, Ordering::Relaxed);
    assert_eq!(OUTPUT_CONTENT_WIDTH.load(Ordering::Relaxed), 0);
}

#[test]
fn emitter_textdelta_still_appends_after_width_wiring() {
    let output_view = OutputView::new();
    let handle = output_view.shared_handle();
    let status = StatusBarState::shared();
    let emitter = build_test_emitter(handle, Arc::clone(&status));
    emitter(StatusEvent::TextDelta("Hello table".to_string()));
    let snapshot = output_view.snapshot();
    assert!(snapshot.contains("Hello table"), "TextDelta 应被追加到输出缓冲");
}
```
- [ ] **Step 2: 运行确认失败**

```
cargo test -p rusty-claude-cli content_width_static 2>&1 | tail -15
```

Expected: `cannot find value OUTPUT_CONTENT_WIDTH in this scope`

- [ ] **Step 3: 实现**

3a. 顶部导入追加（L11 `use std::sync::{Arc, Mutex};` 之后）：

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
```

3b. 模块级静态（放在 `wrap_line_to_display_lines` 定义之前，约 L300 区域）：

```rust
/// 当前输出内容区宽度（显示列），由 draw 循环每帧更新；
/// emitter 在渲染 markdown 表格前读取，保证表格宽度匹配内容区。
static OUTPUT_CONTENT_WIDTH: AtomicUsize = AtomicUsize::new(0);
```

3c. draw 循环（L1014 `let content_width = main_area.width as usize;` 之后）：

```rust
let content_width = main_area.width as usize;
OUTPUT_CONTENT_WIDTH.store(content_width, Ordering::Relaxed);
```

3d. emitter TextDelta 分支（L2899-2908，push 前）：

```rust
if let Ok(mut ms) = markdown_state_for_closure.lock() {
    ms.set_max_width(Some(OUTPUT_CONTENT_WIDTH.load(Ordering::Relaxed)));
    if let Some(rendered) = ms.push(renderer, &text) {
        if let Ok(mut buf) = output_handle.lock() {
            buf.append(&rendered);
        }
    }
}
```

3e. emitter MessageStop flush 分支（L3026-3035，flush 前）：

```rust
if let Ok(mut ms) = markdown_state_for_closure.lock() {
    ms.set_max_width(Some(OUTPUT_CONTENT_WIDTH.load(Ordering::Relaxed)));
    if let Some(rendered) = ms.flush(renderer) {
        if let Ok(mut buf) = output_for_closure.lock() {
            buf.append(&rendered);
        }
    }
}
```

- [ ] **Step 4: 运行确认通过（含 emitter 回归）**

```
cargo test -p rusty-claude-cli emitter_ 2>&1 | tail -25
```

Expected: 新增 2 个测试 + 现有 `emitter_*` 测试全部 PASS

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/tui/app.rs
git commit -m "fix(tui): wire content width into markdown rendering"
```

---

### Task 6: 清理临时示例 + 提交文档 + 全量验证

**Files:**
- Delete: `rust/crates/rusty-claude-cli/examples/repro_md.rs`
- Commit: `docs/2026-08-05-tui-md-table-wrap-design.md`、`docs/plans/2026-08-05-tui-md-table-wrap.md`

- [ ] **Step 1: 删除临时复现脚本**

```bash
rm rust/crates/rusty-claude-cli/examples/repro_md.rs
git rm rust/crates/rusty-claude-cli/examples/repro_md.rs 2>/dev/null || true
```

- [ ] **Step 2: 提交设计文档与实现计划**

```bash
git add docs/2026-08-05-tui-md-table-wrap-design.md docs/plans/2026-08-05-tui-md-table-wrap.md
git commit -m "docs(tui): 表格对齐+换行修复设计与实现计划"
```

- [ ] **Step 3: 全量测试**

```
cargo test -p rusty-claude-cli 2>&1 | tail -30
```

Expected: 全部测试 PASS（`test result: ok`，无失败）

- [ ] **Step 4: clippy 检查**

```
cargo clippy -p rusty-claude-cli --all-targets -- -D warnings 2>&1 | tail -30
```

Expected: 无 warning。若有 `unused_mut`/`needless_borrow` 等，按提示修正。

- [ ] **Step 5: 格式检查**

```
bash scripts/fmt.sh --check 2>&1 | tail -20
```

Expected: 通过；不通过则 `bash scripts/fmt.sh` 后再检查。

- [ ] **Step 6: 手动冒烟（可选，TUI 需要真实终端）**

```
cargo run -p rusty-claude-cli --example repro_md 2>/dev/null || true   # 已删除,忽略
```

改用单元测试覆盖即可；如需真实终端验证，运行 `cargo run -p rusty-claude-cli --bin claw -- --tui` 后让模型回复含长 URL 表格与长英文段落，目视确认对齐与词边界。

- [ ] **Step 7: Commit 清理**

```bash
git add -A rust/crates/rusty-claude-cli/examples/
git commit -m "chore: remove temporary repro example"
```

---

## 实现可行性评审（逐项推演）

1. **签名兼容性**：`markdown_to_ansi_with_width` 为新方法，`markdown_to_ansi` 保留原签名；`MarkdownStreamState::push/flush` 签名不变（仅加字段 + 新构造器/`set_max_width`）→ streaming.rs（L513, L648）与现有测试零改动 ✅
2. **参数来源**：`max_width` 三来源——TUI 原子值（每次 TextDelta/MessageStop 读取）、非 TUI `None` → `resolve_table_max_width` 查 `crossterm::terminal::size()`、失败兜底 100 ✅
3. **数据传递链**：draw（`OUTPUT_CONTENT_WIDTH.store`）→ emitter（`load` → `set_max_width`）→ `push/flush` → `markdown_to_ansi_with_width` → `render_markdown_with_width` → `RenderState.table_max_width` → `render_table`，逐层显式传递 ✅
4. **判定优先级**：表格行保护（首 grapheme == "│"，在宽度检查后、词折行前）> 词边界折行 > 超宽 token 硬拆。表格行绝不词折行 ✅
5. **retry/重入**：`markdown_to_ansi` 每个流式片段调用一次；wrap 缓存（`cached_wrap_ptr`/`cached_wrap_width`）逻辑不变；原子为 Relaxed load/store，开销可忽略 ✅
6. **冲突处理**：resize 使已渲染表格超宽 → 表格行原样返回、Paragraph 右缘裁剪，左对齐保持，避免 mid-cell 断行 ✅
7. **与现有系统重叠**：`wrap_plain_text`（输入框，光标对齐依赖）、`highlight_code`（ToolCard）、sticky/scroll、display_breaks 语义均不受影响 ✅
8. **失败路径**：`terminal::size()` 非终端返回 Err → 100 兜底；`parse_ansi_units` 对非 SGR 转义按零宽透传不 panic；`ansi_to_tui` 失败沿用现有 fallback ✅
9. **构造点破坏扫描**：`RenderState` 仅 `render_markdown_with_width` 内 `default()` 构造（测试不直接构造）——新增 `Option` 字段默认 None，无破坏；`MarkdownStreamState::default()` 调用点：tui/app.rs:2893、streaming.rs:513、render.rs 测试多处——新增 `Option` 字段全部兼容；无 `..Default::default()` 结构体更新语法调用点 ✅
10. **成本估算**：render.rs 新增 ~230 行（辅助函数 + 表格重写 + 流式宽度）+ 测试 ~120 行；tui/app.rs 重写 ~90 行 + 原子接线 ~12 行 + 测试 ~80 行；含边界 case（空单元格、CJK、畸形 ANSI、resize）✅

## 风险与缓解

| 风险 | 缓解 |
| --- | --- |
| 表格重写破坏 `renders_tables_with_alignment` 精确断言 | Task 2 Step 4 显式回归该测试；短表格（自然宽 ≤ 目标宽）输出字节一致 |
| ANSI 样式折行丢失/损坏 | `parse_ansi_units` 状态化前缀 + 行尾补 reset（`flush_ansi_line`），Task 1 测试覆盖 |
| 词边界折行改变 display 行数影响滚动/计数 | `display_breaks` 按 entry 重新统计（`wrap_lines_with_breaks` 逻辑不变），语义保持 |
| 零宽字符/emoji 宽度误差 | 沿用 `UnicodeWidthStr::width`，与现有 `visible_width`/wrap 一致 |
| 测试环境无终端 | `terminal::size()` Err → 默认 100，所有测试确定性通过 |
