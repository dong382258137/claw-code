# TUI 输出智能化与人性化交互方案

**版本**: v1.0 (2026-08-02)
**状态**: 已实现 (2026-08-03)
**范围**: TUI 输出渲染层 + 工具结果 schema + system prompt

## 1. 背景与问题

### 1.1 用户反馈
1. **折叠粗糙**：模型想让用户看的内容被折叠起来
2. **MD 渲染失效**：AI 回复的 markdown 源码(`#`/`**`/` ``` `)以纯文本原样显示

### 1.2 根因（基于代码事实）

**MD 渲染失效——根本没接线**：
- `app.rs:2603-2606` `TextDelta` 直接 `buf.append(&text)` 塞原始 markdown
- `render.rs` 有完整 pulldown-cmark + syntect 渲染器，但 TUI 生产路径零调用
- `app.rs:31-34` 注释 "Phase 3.2: TerminalRenderer is used to convert markdown → ANSI" — 半成品中断
- 非 TUI 路径 `streaming.rs:1037` 正确调用了 `markdown_to_ansi`，可作参照

**折叠粗糙——单一阈值 + 无语义感知**：
- `tool_card.rs:12-14` 全局常量 `COLLAPSE_THRESHOLD=5` / `PREVIEW=3`，所有工具共用
- `output_view.rs:350` `complete_tool_card` 无条件 `*collapsed=true`
- `tool_card.rs:88` `is_error` 只改图标(❌/✅)，不影响折叠策略
- `detect_language_for_tool`(`tool_card.rs:147`) 已能识别 JSON/code/diff，但只喂高亮，不反馈折叠

**bash 错误识别失效——is_error 对 bash 永远 false**：
- `execute_bash` 成功返回 `Ok(BashCommandOutput)`，`interrupted:true` 在输出结构内部
- `run_bash`(`tools/lib.rs:2755`) → `Ok(String)`
- `conversation.rs:2070` `is_error = result.is_err()` → bash 执行成功时 is_error 永远 false
- `returnCodeInterpretation` 字段 TUI 层完全没读

## 2. 设计原则

1. **信息重要性 > 内容长度**：展现层级由意图重要性决定，不靠行数
2. **模型意图通道**：让模型表达"这很重要"，启发式作 fallback
3. **渐进式披露**：L1 摘要 → L2 预览 → L3 完整，三级由用户逐步展开
4. **错误不丢失**：错误不抢滚动但可发现，支持快捷跳转
5. **上下文连续性**：翻历史时不被新输出打断，提示条引导

## 3. 详细设计

### 3.1 模型意图通道：`emphasis` 字段

工具结果 schema 新增 `emphasis: Option<"high"|"normal"|"low">`：
- `high` → P0：永不折叠，视觉突出
- `normal` → P1/P2：按长度+类型折叠
- `low` → P3：单行摘要
- 不填 → 启发式 fallback

**启发式 fallback 规则**（解析 `returnCodeInterpretation` 字段，而非依赖 is_error）：

```
returnCodeInterpretation 值         → priority
─────────────────────────────────────
"interrupted"                       → low   (用户 Ctrl+C 取消)
"idle.timeout"                      → high  (死锁/挂起，需关注)
"timeout" / "test.hung"             → high  (超时被杀，需关注)
"exit_code:0" 或 None               → normal (正常)
"exit_code:N" (N≠0)                 → high  (命令失败)
is_error=true (spawn 失败等)         → high
```

**其他工具启发式**：
- `edit_file` 有 diff → normal
- `grep/glob` → normal
- JSON 且 >20 行 → low
- 输出 ≤8 行 → normal (默认展开)
- 兜底 → normal

**prompt 侧**：system prompt 加一段，教模型 bash 工具的 emphasis 用法——错误/关键发现标 high，常规操作不填(走 fallback)，纯确认标 low。

### 3.2 信息分级 → 默认展现层级映射

| priority | 内容 | 默认层级 | 折叠行为 |
|----------|------|----------|----------|
| P0 | AI 文本 / error / high 标记 | L3 完整 | 永不折叠 |
| P1 | diff / 匹配 / 短输出 | L3(≤8行) / L2(>8行) | 中等长度默认展开 |
| P2 | 长 JSON / 过程数据 | L1 摘要 | 折叠+语义摘要 |
| P3 | 成功确认 / 空输出 / interrupted | L1 摘要 | 单行 |

**关键原则**：AI 文本本身永远是 P0 — 模型写的每个字都是给用户看的，不该被任何折叠逻辑触碰。

### 3.3 L1 摘要生成（按工具，`tool_card.rs` 新增）

```
bash(ok)          → ✅ · {N}行 · {末行截断60}
bash(error)       → ❌ · exit {code} · {末行截断60}
bash(interrupted) → ⏹ · 已取消
edit/write        → ✏️ {path} · {hunks}处修改
read_file         → 📄 {path} · {numLines}行
grep              → 🔎 {pattern} · {num_matches}处 / {num_files}文件
glob              → 🌐 {pattern} · {num_files}文件
web_fetch/search  → 🌐 · {N}行
兜底              → 📦 {tool_name} · {输出行数}行
```

关键：bash 取**末行**而非首行（bash 输出结构是过程在前、结论在后）。

### 3.4 错误处理：不抢滚动 + 跳转提示

- `OutputView` 维护 `error_entries: Vec<usize>`（entry 索引）
- `complete_tool_card` 时若 is_error 或 priority=high 且是错误 → push 索引
- 状态栏显示 `⚠ {n} 错误`
- 快捷键：
  - `E`（大写）+ `buffer().is_empty()` 守卫 → 跳转到下一个 error entry 并自动展开
  - `End` 键（无守卫，功能键不插入字符）→ 跳回底部 + 清零新输出计数
- error entry 不被 trim 淘汰（当前 trim 按字节，需加保护标记）

**End 键选型理由**：
- End 是功能键，不是字母键，不需空闲守卫，用户正在打字时也能用
- 当前 InputLine 无光标移动功能(无 Home/End/Ctrl+A/Ctrl+E)，无语义冲突
- End 语义暗示"到底部"，符合跳转底部的直觉

### 3.5 智能 auto-follow

**移除** `app.rs:1600` submit 时 `scroll_offset = None` 的强制重置。

**新增机制**：
- 计数器 `new_output_lines_since_last_view: usize`
- follow 态(`scroll_offset.is_none()`)时持续清零
- manual 态(`scroll_offset.is_some()`)时随新输出累加
- 显示条件：`scroll_offset.is_some() && new_output_lines > 0`
- 显示位置：边框标题区，与现有 `[scroll -N]` 并列

**提示条文案**：`[↓ {N} 行新输出]`
- 复用 `scroll_label` 机制，无需额外渲染区域
- 与 `[scroll -N]` 格式对称（方括号 + 描述 + 数字）
- 中点 `·` 改为空格分隔（避免 CJK 宽度问题）
- 不加操作提示（End 随时可用，Help 浮层会说明）

**示例**：
```
follow 态:  "输出"
manual 态:  "输出 [scroll -15]"
manual+新:  "输出 [scroll -15] [↓ 8 行新输出]"
```

**End 键行为**：`scroll_offset = None` + 清零计数器
**E 键行为**：跳转下一个 error entry + 自动展开

### 3.6 MD 渲染接线（P0 文本可读性基础）

AI 文本是 P0 永不折叠 — 若不接 MD 渲染，P0 文本仍是一堆 `#`/`**` 字符。必须包含：

- `app.rs:2603` `TextDelta` 改用 `render.rs:630` `StreamingMarkdownRenderer`（已有，处理增量边界 + pending buffer）
- syntect 输出经 `ansi_to_tui` 转 `ratatui::Text<Spans>`，规避 crossterm ANSI 反射
- 代码块单独走 syntect→Spans 路径，规避反射问题（`tool_card.rs:15-20` 顾虑点）
- 流式增量必须用 `StreamingMarkdownRenderer` 而非直接 `markdown_to_ansi`（避免半个 fence 渲染错乱）

## 4. 实现拆解

8 个 commit，依赖顺序：

| # | 改动 | 文件 | 依赖 | commit |
|---|------|------|------|--------|
| 1 | tool schema 加 emphasis 字段 | `runtime/src/bash.rs` `BashCommandOutput` 等 | 无 | `a80013de` |
| 2 | prompt 教模型用 emphasis | `runtime/src/prompt.rs` | #1 | `b0d4bd49` |
| 3 | OutputEntry 加 priority + 启发式 | `tui/output_view.rs` | #1 | `1b713e02` |
| 4 | 折叠按 priority 分档 + L1 摘要 | `tui/tool_card.rs` | #3 | `16af4c68` |
| 5 | error 索引 + E/End 跳转 | `tui/output_view.rs`, `tui/app.rs` | #3 | `d25a3b31` |
| 6 | auto-follow 冻结 + 新输出提示 | `tui/app.rs` | 无 | `d25a3b31` |
| 7 | MD 渲染接线（MarkdownStreamState） | `tui/app.rs` | 无 | `dc9269d0` |
| 8 | trim 保护 error entry | `tui/output_view.rs` | #5 | `2fd1dd59` |

- #1-2 是 runtime+prompt 层
- #3-8 是 TUI 层
- #5+#6 合并为一个 commit（共享 app.rs 改动）
- 全部 485 测试通过，无回归

## 5. 验证计划

每个 commit 验证：
- `cargo check -p runtime` / `cargo check -p rusty-claude-cli` 编译通过
- `cargo test -p runtime --lib` / `cargo test -p rusty-claude-cli --lib` 无回归
- 新增单元测试覆盖关键逻辑

端到端验证（#8 完成后）：
- bash 命令成功(短/长) → 正常显示/折叠+摘要
- bash 命令失败 → 红边框+不折叠+状态栏 ⚠ 提示
- bash Ctrl+C 取消 → ⏹ 已取消(不抢滚动)
- AI 回复 markdown → 正确渲染(标题/列表/代码块/加粗)
- 翻历史时新输出 → 不抢滚动 + 提示条显示
- End 跳底部 / E 跳错误 → 正确跳转

## 6. 风险与缓解

| 风险 | 缓解 |
|------|------|
| syntect ANSI 反射污染 InputLine | 用 `ansi_to_tui` 转 `Text<Spans>` 而非直接输出 ANSI 字节 |
| 模型不正确使用 emphasis | 启发式 fallback 兜底；prompt 给清晰示例 |
| 流式 MD 渲染半个 fence 错乱 | 用 `StreamingMarkdownRenderer` 找安全切分点 |
| trim 淘汰 error entry | 加 `protected` 标记，trim 跳过 |
| End 键未来与光标移动冲突 | 当前无光标移动功能；未来若加可改 Ctrl+End |

## 7. 不在本方案范围

- normal/insert 模式切换（vim 式）
- 多光标支持
- 鼠标拖拽选择
- 输出内容搜索（/ 搜索）
- 会话级错误持久化
