# TUI 界面调整：缓存命中率 + Git 移至底栏、侧栏默认关闭

**Goal:** 把 TUI 侧栏中的「缓存命中率」「Git 显示」移到单行底栏，并将右侧侧栏默认关闭（F2 / Ctrl+B 手动打开），保持界面清爽。

**Architecture:** 底栏 `StatusBar`（`tui/status_bar.rs`）在既有 sections 扁平化渲染中追加两段：Git 段（cwd 之后）与缓存命中率段（ctx 进度条之后）；数据全部取自既有 `StatusBarState`（`git_branch`/`git_status`/`cumulative_usage`/`turn_usage`），新增 `StatusBarState::cache_hit_rate()` 辅助方法作为唯一命中率计算源。侧栏 `tui/sidebar.rs` 删除分支/Git 状态/命中率三行（保留会话/权限/目标/技能/工具/用量明细），`tui/app.rs` 将 `sidebar_visible` 初始值改为 `false`。

**Tech Stack:** Rust, ratatui, crossterm, `full-tui` cargo feature。

---

## 现状分析表

| 组件 | 位置 | 现状 | 验证标记 |
|---|---|---|---|
| 底栏状态栏 | `rust/crates/rusty-claude-cli/src/tui/status_bar.rs:146-380` | 单行 sections 扁平化渲染：model/effort/🪙/#轮次/cwd/cost/ctx进度条/timer/version | ✅ 已读全文 |
| 底栏扁平化 | 同文件 `:352-366` | 超宽自动截断末尾段（已有降级逻辑） | ✅ |
| `StatusBarState` 字段 | 同文件 `:33-66` | 已有 `git_branch`、`git_status`、`cumulative_usage`、`turn_usage` | ✅ |
| `StatusBarState` impl | 同文件 `:68-133` | `total_tokens`/`context_tokens`/`reset_turn`/`finish_turn` | ✅ |
| 侧栏会话区 | `rust/crates/rusty-claude-cli/src/tui/sidebar.rs:125-180` | 分支 / 会话 / 权限 / 目标? / Git 工作区状态 | ✅ |
| 侧栏用量区 | 同文件 `:284-420` | 用量/状态/令牌/消息/工具/输入/输出/命中缓存/未命中/命中率（10 行） | ✅ |
| 侧栏默认显示 | `rust/crates/rusty-claude-cli/src/tui/app.rs:655` | `terminal.size().width >= 88` 时自动显示 | ✅ |
| 侧栏切换 | `tui/app.rs:1647`（处理）+ `:2574,2622`（F2/Ctrl+B） | 已有 toggle，本次不动 | ✅ |
| 帮助浮层 | `tui/app.rs:2912` | `("F2 / Ctrl+B", "切换右侧侧栏")` | ✅ |
| git 数据来源 | `tui/app.rs:3424-3470` `sync_status_from_cli_inner` | turn 完成时刷新，git_status 3s 缓存；底栏与侧栏同源，无需改数据链 | ✅ |
| 非 TUI 命中率 | `doctor.rs:422-432` `hit_rate_pct` | `claw doctor --cache-stats` 独立功能，不受影响 | ✅ |

## 实现可行性推演

1. **签名兼容性**：`cache_hit_rate(&self) -> Option<f64>` 为同步 &self 方法，在 `StatusBar::render`（持 `self.state: &StatusBarState`）内直接调用，无 async/lifetime 问题。✅
2. **参数来源**：全部来自 `self.state`（git_branch/git_status/cumulative_usage/turn_usage），无新增入参。✅
3. **数据传递链**：git/cache 数据已在 `StatusBarState` 中，turn 完成时由 `sync_status_from_cli` 刷新，`StatusBar` 每帧读取——与侧栏现状一致，无数据丢失。✅
4. **判定优先级**：Git 段 `git_status` match 顺序：`"clean"`→绿；`""`（非 git/未获取）→灰；其他（dirty 摘要）→黄。缓存段：`None`→不渲染；`>=0.85`→绿；`>=0.60`→黄；否则红。✅
5. **retry/重入**：`render` 每帧调用，`cache_hit_rate` 纯算术零成本；git_status 上游已 3s 缓存，无重复开销。✅
6. **冲突处理**：纯展示逻辑，无外部输入冲突。✅
7. **与现有系统重叠**：命中率/分支/Git 状态从侧栏移除后，底栏成为唯一展示位，无重复渲染。✅
8. **失败路径**：非 git 仓库→`git_branch` 空→Git 段隐藏；无缓存数据→缓存段隐藏；窄终端→末尾段自动截断（沿用 `:352-366` 逻辑）。✅
9. **构造点破坏扫描**：只新增方法、不新增字段，所有 `StatusBarState` 构造点（`..Default::default()` 模式）不受影响。`sidebar.rs` 测试 `make_state()`（`:456-475`）与 `status_bar.rs` 测试均保持编译。✅
10. **成本估算**：status_bar +~35 行、sidebar -~30 行、app.rs -2 行、mod.rs ~2 行、测试 +~70 行，总计约 100 行，不含 prompt 工程。✅

---

### Task 1: 底栏新增 Git 段 + 缓存命中率段

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/status_bar.rs:77`（impl 内插入辅助方法）
- Modify: `rust/crates/rusty-claude-cli/src/tui/status_bar.rs:210`（P2 cwd 与 P3 cost 之间插入 Git 段）
- Modify: `rust/crates/rusty-claude-cli/src/tui/status_bar.rs:280`（P4 ctx 与 P5 timer 之间插入缓存段）
- Test: 同文件 `mod tests`（追加 5 个测试）

- [ ] **Step 1: 新增 `cache_hit_rate()` 辅助方法**

在 `total_tokens()` 结束（`status_bar.rs:77` 的 `}`）之后、`context_tokens()` 注释之前插入：

```rust
    /// 缓存命中率（0.0~1.0），口径与侧栏原算法一致：
    /// 命中 = cache_read，未命中 = cache_creation，均为累计 + 当前轮 delta。
    /// 无缓存数据（命中 + 未命中 = 0）时返回 None，调用方不渲染该段。
    pub(crate) fn cache_hit_rate(&self) -> Option<f64> {
        let hit = (self.cumulative_usage.cache_read_input_tokens as u64)
            + (self.turn_usage.cache_read_input_tokens as u64);
        let miss = (self.cumulative_usage.cache_creation_input_tokens as u64)
            + (self.turn_usage.cache_creation_input_tokens as u64);
        let sum = hit + miss;
        if sum == 0 {
            None
        } else {
            Some(hit as f64 / sum as f64)
        }
    }
```

- [ ] **Step 2: 在 P2 cwd 段之后插入 Git 段**

`status_bar.rs` P2 cwd 段（`:201-209`，以 `]);` 结束）与 P3 Cost 注释（`:212`）之间插入：

```rust
        // P2.5: Git 分支 + 工作区状态（从侧栏移到底栏；非 git 仓库时不显示）
        if !self.state.git_branch.is_empty() {
            let (git_style, dirty_suffix) = match self.state.git_status.as_str() {
                // 工作区干净：分支绿色
                "clean" => (
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                    String::new(),
                ),
                // git_status 为空（非 git 目录 / 未获取）：分支灰色
                "" => (Style::default().fg(Color::DarkGray), String::new()),
                // 有改动：分支黄色，追加摘要（如 ±3）
                summary => (
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                    format!(" {summary}"),
                ),
            };
            sections.push(vec![
                Span::styled(" │ ", style_dim),
                Span::styled("⎇ ", style_dim),
                Span::styled(
                    format!("{}{}", self.state.git_branch, dirty_suffix),
                    git_style,
                ),
            ]);
        }
```

- [ ] **Step 3: 在 P4 ctx 段之后插入缓存命中率段**

`status_bar.rs` P4 ctx 段（`:271-279`，以 `]);` 结束）与 P5 Streaming timer 注释（`:281`）之间插入：

```rust
        // P4.5: 缓存命中率（从侧栏移到底栏；无缓存数据时不显示）
        if let Some(rate) = self.state.cache_hit_rate() {
            let cache_style = if rate >= 0.85 {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if rate >= 0.60 {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::BOLD)
            };
            sections.push(vec![
                Span::styled(" │ ", style_dim),
                Span::styled("缓存 ", style_dim),
                Span::styled(format!("{:.0}%", rate * 100.0), cache_style),
            ]);
        }
```

- [ ] **Step 4: 追加测试**

`status_bar.rs` `mod tests` 末尾追加 5 个测试：

```rust
    #[test]
    fn status_bar_shows_git_and_cache_sections() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "deepseek-v4-pro".to_string(),
            cwd: "~/claw".to_string(),
            git_branch: "main".to_string(),
            git_status: "±3".to_string(),
            turn_count: 3,
            cumulative_usage: TokenUsage {
                input_tokens: 1_000,
                output_tokens: 500,
                cache_creation_input_tokens: 1_000, // miss
                cache_read_input_tokens: 9_000,     // hit → 90%
                ..Default::default()
            },
            ..Default::default()
        };
        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(content.contains("main"), "branch: {content}");
        assert!(content.contains("±3"), "git dirty summary: {content}");
        assert!(content.contains("90%"), "cache rate: {content}");
    }

    #[test]
    fn status_bar_hides_git_when_not_in_repo() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "test-model".to_string(),
            cwd: "~".to_string(),
            ..Default::default() // git_branch 为空
        };
        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(!content.contains('⎇'), "no git section: {content}");
        assert!(!content.contains("main"), "no branch: {content}");
    }

    #[test]
    fn status_bar_hides_cache_when_no_cache_usage() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let state = StatusBarState {
            model: "test-model".to_string(),
            cwd: "~".to_string(),
            cumulative_usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            ..Default::default()
        };
        let widget = StatusBar { state: &state };
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let content: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(!content.contains("缓存"), "no cache section: {content}");
    }

    #[test]
    fn cache_hit_rate_returns_none_without_cache_data() {
        let state = StatusBarState::default();
        assert_eq!(state.cache_hit_rate(), None);
    }

    #[test]
    fn cache_hit_rate_sums_cumulative_and_turn() {
        let mut state = StatusBarState::default();
        state.cumulative_usage.cache_read_input_tokens = 80;
        state.cumulative_usage.cache_creation_input_tokens = 10;
        state.turn_usage.cache_read_input_tokens = 10;
        state.turn_usage.cache_creation_input_tokens = 0;
        // hit = 90, miss = 10 → 0.90
        assert!((state.cache_hit_rate().unwrap() - 0.90).abs() < 1e-9);
    }
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p rusty-claude-cli --features full-tui tui::status_bar`（仓库根目录执行；若 feature 名不同，先在 `rust/crates/rusty-claude-cli/Cargo.toml` 确认，见 Task 4 Step 0）
Expected: 全部 PASS（含既有测试）

- [ ] **Step 6: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/tui/status_bar.rs
git commit -m "feat(tui): 底栏新增 Git 分支/工作区状态与缓存命中率显示"
```

---

### Task 2: 侧栏移除 Git 显示与命中率

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/sidebar.rs:1-17`（模块 doc）
- Modify: `rust/crates/rusty-claude-cli/src/tui/sidebar.rs:88-90`（布局注释）
- Modify: `rust/crates/rusty-claude-cli/src/tui/sidebar.rs:125-180`（render_session_section）
- Modify: `rust/crates/rusty-claude-cli/src/tui/sidebar.rs:284-420`（render_usage_section）
- Modify: `rust/crates/rusty-claude-cli/src/tui/sidebar.rs:155-160`（reserve_for_bottom 常量）
- Test: 同文件 `mod tests`（`:530-556` 追加断言）

- [ ] **Step 1: 更新模块 doc**

`sidebar.rs:1-17` 中 `//! - Live token usage breakdown (input/output/cache hit/cache miss/cache hit rate)` 改为：

```rust
//! - Session metadata (session id, permissions, goal)
//! - Current-turn skill invocations
//! - Current-turn tool call history (name + success/error marker)
//! - Session statistics (message count, success rate, duration)
//! - Live token usage breakdown (input/output/cache hit/cache miss; 命中率已移到底栏)
//! - Streaming timer
```

（原文中 `branch, session id, permissions, goal, git status` 也同步改为 `session id, permissions, goal`）

- [ ] **Step 2: 更新布局注释**

`sidebar.rs:88-90`：

```rust
    // Section layout (top→bottom):
    //  Session: 2-4 lines (session, permissions, goal?)  ← 分支/Git 已移到底栏
    //  Skills:  dynamic (if any skill invocations)
    //  Tools:   dynamic
    //  Stats+Usage: remaining → 2 stat lines + usage details
```

- [ ] **Step 3: 精简 `render_session_section`**

`sidebar.rs:125-180` 整个函数体替换为（删除 分支 行与 Git 工作区状态块）：

```rust
fn render_session_section(area: Rect, buf: &mut Buffer, state: &StatusBarState) {
    // 分支/Git 工作区状态 已移到底栏，此处不再显示。
    let mut lines = vec![
        Line::from(vec![
            Span::styled("会话 ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.session_id),
        ]),
        Line::from(vec![
            Span::styled("权限 ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.permission_mode),
        ]),
    ];
    if !state.goal_badge.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("目标 ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.goal_badge),
        ]));
    }
    let paragraph = Paragraph::new(lines).alignment(Alignment::Left);
    paragraph.render(area, buf);
}
```

- [ ] **Step 4: 移除 `render_usage_section` 的命中率行**

`sidebar.rs:284-420`：
1. 删除 `hit_rate` 与 `hit_rate_color` 的计算块（原 `:294-310`），只保留 `hit_total`/`miss_total`/`cache_sum`（供 命中缓存/未命中 两行使用；`cache_sum` 若不再被引用一并删除）；
2. 删除 `lines` 向量末尾的「命中率」`Line::from(vec![Span::styled("命中率  ", ...), ...])`（原 `:411-420`）；
3. 同时删除顶部注释 `// 缓存统计 (命中= cache_read, 未命中= cache_creation)` 下方不再使用的变量。

删除后 `lines` 末尾两行为（保持不变）：

```rust
        Line::from(vec![
            Span::styled("命中缓存", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} tokens", hit_total)),
        ]),
        Line::from(vec![
            Span::styled("未命中  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} tokens", miss_total)),
        ]),
    ];
```

- [ ] **Step 5: 调整 `reserve_for_bottom` 常量**

`sidebar.rs:158-159`：

```rust
    // Tools section: carve remaining space, leaving at least 9 rows for stats+usage
    let reserve_for_bottom = 11u16; // 9 usage lines (命中率已移底栏) + 1 top border + 1 margin
```

- [ ] **Step 6: 追加侧栏测试断言**

`sidebar.rs` `usage_section_shows_stats_lines` 测试（`:530-556`）末尾追加：

```rust
        assert!(
            !content.contains("命中率"),
            "cache hit rate moved to bottom bar: {content}"
        );
```

（该测试仍断言 `8轮` / `12次` / `83%`，均不受影响）

- [ ] **Step 7: 运行测试**

Run: `cargo test -p rusty-claude-cli --features full-tui tui::sidebar`
Expected: 全部 PASS

- [ ] **Step 8: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/tui/sidebar.rs
git commit -m "refine(tui): 侧栏移除 Git/命中率显示（已移至底栏），保留会话/工具/用量明细"
```

---

### Task 3: 侧栏默认关闭 + 帮助浮层文案

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/app.rs:652-655`
- Modify: `rust/crates/rusty-claude-cli/src/tui/app.rs:2912`
- Modify: `rust/crates/rusty-claude-cli/src/tui/mod.rs:3-11`

- [ ] **Step 1: 默认值改为 false**

`app.rs:652-655`：

```rust
    // Sidebar: hidden by default for a clean interface, toggleable via
    // F2 / Ctrl+B. Holds a shared tool-history mirror so the sidebar
    // can show live tool-call progress during a streaming turn.
    let mut sidebar_visible: bool = false;
```

（原为 `terminal.size().map(|s| s.width >= 88).unwrap_or(false)`，删除对 `terminal` 的依赖——需确认 `terminal` 在该作用域是否还有其他用途，若仅此一处则保留变量本身即可，不影响编译。）

- [ ] **Step 2: 帮助浮层文案**

`app.rs:2912`：

```rust
        ("F2 / Ctrl+B", "打开/关闭右侧侧栏（默认隐藏，查看工具状态）"),
```

- [ ] **Step 3: 更新 tui 模块 doc**

`tui/mod.rs:3-11` 中 `//! - A persistent status bar showing model, cwd, branch, tokens, cost` 改为：

```rust
//! - A persistent status bar showing model, cwd, git, cost, ctx progress,
//!   cache hit rate, streaming timer, version
```

- [ ] **Step 4: 运行测试**

Run: `cargo test -p rusty-claude-cli --features full-tui`
Expected: 全部 PASS（无测试依赖 sidebar 默认开）

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/tui/app.rs rust/crates/rusty-claude-cli/src/tui/mod.rs
git commit -m "feat(tui): 侧栏默认关闭（F2/Ctrl+B 手动打开），保持界面清爽"
```

---

### Task 4: 全量验证

- [ ] **Step 0: 确认 feature 名**

Read: `rust/crates/rusty-claude-cli/Cargo.toml` 中 `[features]` 段的 full-tui feature 名（预期 `full-tui`）
Expected: 找到 `full-tui = [...]` 声明

- [ ] **Step 1: 全量测试**

Run（仓库根 `rust/` 下）: `cargo test --workspace`
Expected: 全部 PASS（含 31+ 既有测试与新增 5+1 个测试）

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: 无警告

- [ ] **Step 3: 手工冒烟（可选）**

Run: `cargo run -p rusty-claude-cli -- --tui`（或实际入口，见 `rusty-claude-cli/Cargo.toml` bin 名）
Expected: TUI 启动后侧栏不显示；底栏显示 `⎇ main ±3`（dirty 时黄色）与 `缓存 XX%`；按 F2 侧栏出现，再按 F2 消失

- [ ] **Step 4: 更新收益清单文档（如适用）**

若 `docs/2026-08-09-design-gaps-benefit-list.md` 有 TUI 布局条目需要同步，追加一行本次改动说明；否则跳过。

---

## Self-Review 结果

- **Spec coverage**：缓存命中率→底栏（Task 1 Step 3）、Git→底栏（Task 1 Step 2）、侧栏默认关闭（Task 3 Step 1）、保留侧栏缓存明细（Task 2 仅删命中率行）—— 全部覆盖 ✅
- **Placeholder scan**：无 TBD/TODO，所有代码块为完整可编译片段 ✅
- **Type consistency**：`cache_hit_rate()` 在 Task 1 定义，Task 1 Step 3 与 Task 2（引用同源）使用一致；无跨任务命名漂移 ✅
- **代码事实核查**：所有引用行号已通过 Read/Grep 验证（见现状分析表）✅
- **构造点破坏扫描**：仅新增方法不新增字段，`..Default::default()` 构造点零破坏 ✅
