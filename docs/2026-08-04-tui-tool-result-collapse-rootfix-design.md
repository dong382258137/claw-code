# TUI 工具结果折叠根治方案 — 内容提取 + 语义分类器

**版本**: v1.0 (2026-08-04)
**状态**: 待评审
**范围**: TUI 输出渲染层（`rusty-claude-cli` crate，`full-tui` feature）
**目标**: 折叠不需要用户看到的工具信息，展开需要用户看到的输出信息（根治，非补丁）

## 1. 背景与根因

用户反馈经多轮优化（`40→8` 行门槛、priority 分档、折叠预览等）后，仍出现"输出内容被当成工具输出折叠起来"。基于代码事实核查，根因有三：

### 1.1 现状分析表（代码事实核查）

| # | 组件 | 位置（文件:行号） | 现状 | 验证 |
|---|------|-------------------|------|------|
| 1 | 工具结果序列化 | `tools/lib.rs:2757-2762`（run_bash）、`tools/lib.rs:2934-2942`（run_read_file）、`tools/lib.rs:3012-3043`（glob/grep/web） | 全部走 `serde_json::to_string_pretty`（`tools/lib.rs:3329-3331`）→ **pretty JSON 信封** | ✅ 已读源码 |
| 2 | BashCommandOutput 结构 | `runtime/src/bash.rs:69-100` | 17 个顶层字段 + 嵌套 sandboxStatus；stdout/stderr 是 JSON 字符串值（`\n` 被转义） | ✅ 已读源码 |
| 3 | 折叠判定兜底启发式 | `tui/output_view.rs:55-96`（compute_priority） | 第 4 步 `result.lines().count() > 8 → P2(折叠)`，统计的是 **JSON 信封行数** | ✅ 已读源码 |
| 4 | 实测行数错位 | — | stdout 3 行 → pretty JSON 38 行 → 恒 P2 恒折叠 | ✅ Python 模拟验证 |
| 5 | TUI 卡片渲染 | `tui/tool_card.rs:88-220`（render_tool_result） | 把原始 JSON 逐行加 `│` 前缀渲染；行数/语言检测/正文全基于 JSON 信封 | ✅ 已读源码 |
| 6 | 非 TUI 路径正确提取 | `tool_display.rs:516-695`（format_bash_result/read/write/edit/glob/grep） | 已有 JSON→内容提取（stdout、file.content、matches），TUI 未复用 | ✅ 已读源码 |
| 7 | 单测形态脱节 | `tui/output_view.rs:1305`（compute_priority_bash_ok_short） | 测试用紧凑 JSON（1 行）→ P1；生产 pretty JSON（38 行）→ P2，**测试盲区** | ✅ 已读源码 + 基线 47 测试全过 |
| 8 | 优先级链前 3 步 | `tui/output_view.rs:60-87` | emphasis → is_error → bash returnCodeInterpretation，逻辑正确，保留 | ✅ 已读源码 |
| 9 | 错误标记保护 | `tui/output_view.rs:855-862`（trim） | error/P0 entry 不被 trim 淘汰，保留 | ✅ 已读源码 |

### 1.2 病灶清单

1. **行数启发式统计 JSON 信封而非真实内容**（`output_view.rs:88-96`）→ 一切工具结果默认折叠。
2. **TUI 渲染原始 JSON 信封**（`tool_card.rs:167-220`）→ 展开也读不懂（stdout 是转义字符串，`sandboxStatus` 等元数据噪音占屏）。
3. **折叠判定与内容语义无关** → 无法区分 `cargo test` 失败（信号）与 `ls` 列表（噪音），设计文档 §3.1"信息重要性 > 内容长度"未在兜底启发式落地。

## 2. 设计原则

1. **内容 > 信封**：折叠判定与渲染全部基于提取后的真实内容，JSON 元数据（sandboxStatus、durationMs 等）不参与行数统计、不上屏。
2. **工具类型语义即信号**：read_file 的内容就是答案；write/edit 结果是确认；glob 是噪音。默认展开层级由工具语义决定，行数只作次级门槛。
3. **内容信号覆盖行数**：bash 输出含错误/测试标记时，无论多长都展开（截断保护 viewport）。
4. **展开但截断**：展开视图上限 60 行 + 省略标记，兼顾可见性与空间（用户已确认此取舍）。
5. **兜底可回退**：未知工具/非 JSON 结果回退原始输出 + 8 行门槛，行为不劣于现状。

## 3. 详细设计

### 3.1 新组件：内容提取层（`tui/tool_card.rs`）

```rust
/// 从工具结果 JSON 信封中提取面向用户的真实内容。
/// 已知工具按结构提取；未知/非 JSON 回退原始输出。
fn extract_tool_output_body(name: &str, output: &str) -> String
```

| 工具（name） | 提取路径 | 备注 |
|--------------|----------|------|
| `bash`/`Bash` | `stdout`；`stderr` 非空时追加 `\n[stderr]\n{stderr}` | 无 stdout/stderr → 空串 |
| `read_file`/`Read` | `file.content` | |
| `grep_search`/`Grep` | `content` | |
| `glob_search`/`Glob` | `filenames` 数组 join `\n` | 噪音类，提取后仍折叠 |
| `WebFetch` | `result` | |
| `edit_file`/`Edit`/`write_file`/`Write` | 空串 | diff 已从 input 渲染（`render_edit_diff`），结果仅确认 |
| `WebSearch`/其他 | 原始 `output` | 未知结构，回退 |

实现要点：
- `serde_json::from_str::<Value>(output)` 失败 → 返回原始 output（兼容非 JSON 工具/错误消息）。
- 与 `tool_display.rs` 的提取路径保持一致（同一份输出 schema），但只提取文本不染 ANSI。
- `pub(crate) fn extract_tool_output_body_public` 包装，供 `output_view.rs` 调用（沿用现有 `_public` 模式）。

### 3.2 新组件：语义感知分类器（重写 `tui/output_view.rs` compute_priority 第 4 步）

保留前 3 步（emphasis → is_error → bash rc 解析，逐行复用现有代码），替换行数兜底为：

```
let body = extract_tool_output_body_public(name, result);
let lines = body.lines().count();
match name {
    "bash" | "Bash" => {
        if contains_error_marker(&body) { P0 }          // error[/error:/panic!/fatal:/FAILED/command not found
        else if body.contains("test result:") { P1 }   // 测试总结
        else if lines > 8 { P2 } else { P1 }
    }
    "read_file" | "Read" => if lines > 40 { P2 } else { P1 },   // 内容是答案，门槛放宽
    "grep_search" | "Grep" => if lines > 50 { P2 } else { P1 },  // 命中即证据
    "edit_file" | "Edit" | "write_file" | "Write"
    | "glob_search" | "Glob" | "Skill" | "TodoWrite"
    | "ToolSearch" | "benchmark_compare" => P3,         // 确认/噪音：单行摘要
    "WebFetch" => if lines > 8 { P2 } else { P1 },
    _ => if lines > 8 { P2 } else { P1 },               // 兜底（未知工具）
}
```

常量：
```rust
/// bash 错误标记（命中即 P0 展开）
const BASH_ERROR_MARKERS: &[&str] = &[
    "error[E", "error:", "panic!", "fatal:", "FAILED", "command not found",
];
/// 展开视图最大行数（用户确认：展开+截断）
const MAX_EXPANDED_LINES: usize = 60;
```

### 3.3 渲染层接线（`tui/tool_card.rs` render_tool_result）

`render_tool_result` 内部改为：

```
let body = extract_tool_output_body(name, output);   // 真实内容
let line_count = body.lines().count();               // 真实行数（标题/N 行 显示此值）
let language = detect_language_for_tool(name, &body); // 检测真实内容（bash 不再误判为 json）
```

展开视图（P0/P1 + collapsed=false）截断保护：
```
若 line_count > MAX_EXPANDED_LINES:
    渲染前 60 行 + "│ …（剩余 N 行省略）" 标记行
否则：现有完整渲染
```

- P2 折叠单行标题、P3 L1 摘要、P0 强制展开分支逻辑不变，仅行数来源改为提取后的 body。
- `render_edit_diff`（diff_prefix）不变——edit 卡片 diff 仍从 input 渲染，与结果提取解耦。
- `detect_language_for_tool` 对 bash 检测 body 是否 JSON：命令本身打印 JSON 才高亮 json，行为更正确。

### 3.4 交互不变

- Tab/鼠标点击切换折叠：逻辑不变（`collapsed` 布尔位语义不变）。
- J/K 跳转 AI 回复锚点、E 跳错误：不变（`Text` entry 不受影响）。
- trim 保护 error/P0 entry：不变。

## 4. 测试计划

### 4.1 新增回归测试（消灭测试盲区）

| 测试 | 输入形态 | 断言 |
|------|----------|------|
| `compute_priority_pretty_json_short_stdout` | **pretty JSON**（3 行 stdout → 38 行信封） | `Priority::P1`（不折叠）← 核心回归 |
| `compute_priority_bash_stderr_error` | pretty JSON + stderr 含 `error:` | `Priority::P0` |
| `compute_priority_bash_test_result` | pretty JSON + stdout 含 `test result:` | `Priority::P1` |
| `compute_priority_read_file_expands` | pretty JSON（file.content 20 行） | `Priority::P1` |
| `compute_priority_write_confirms` | pretty JSON | `Priority::P3` |
| `extract_tool_output_body_bash` | pretty JSON | 提取出的 body == stdout |
| `extract_tool_output_body_read_file` | pretty JSON | body == file.content |
| `render_expanded_truncates_at_60` | 100 行 body + P1 | 渲染含前 60 行 + 省略标记，不含第 61 行 |
| `render_tool_result_renders_body_not_json` | pretty JSON | 渲染不含 `"sandboxStatus"`，含真实 stdout 行 |

### 4.2 现有测试更新

- 用 raw 多行文本模拟结果的测试（`compute_priority_long_output` 等）：提取层对非 JSON 回退原始输出 → 断言不变，可保留。
- `complete_tool_card_long_output_shows_collapse_preview`：50 行 raw → P2 折叠单行 → 断言不变。
- `render_tool_result_long_output_collapsed/expanded`：raw 文本 → 提取回退 → 不变。

### 4.3 验证命令

```
cargo test -p rusty-claude-cli --lib -- tui:: 2>&1 | tail
```

## 5. 影响范围与风险

| 项 | 内容 |
|----|------|
| 改动文件 | `rusty-claude-cli/src/tui/tool_card.rs`、`rusty-claude-cli/src/tui/output_view.rs`（+ 各自测试模块） |
| 不动 | `runtime/`（输出 schema 不变）、`tools/`、`prompt.rs`（emphasis 通道保留为额外信号）、`tool_display.rs` |
| 风险 1 | 提取字段路径与工具输出 schema 漂移 → 单测用真实形态 pretty JSON 覆盖主路径，漂移即测红 |
| 风险 2 | 展开 60 行截断后用户无法看到剩余 → 用户已确认此取舍；AI 回复负责总结全文 |
| 风险 3 | P3 后 bash 长输出仍折叠 → 属"过程噪音"设计意图，Tab 可展开 |

## 6. 不在本方案范围

- MD 渲染（AI 回复 markdown，`dc9269d0` 已接线）
- 会话级错误持久化、输出搜索
- 多级渐进披露（L1/L2/L3 三级切换）——当前保持"折叠单行 / 展开 60 行"两级
