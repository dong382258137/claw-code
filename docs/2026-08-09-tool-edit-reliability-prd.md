# PRD：claw 框架工具调用可靠性改进

- **版本**：v1.0
- **日期**：2026-08-09
- **状态**：待评审
- **范围**：`runtime` / `tools` crate 的文件编辑类工具 + bash 工具的调用可靠性
- **来源**：`session-1786253044035-0` 会话工具调用效率审计

---

## 1. 背景与问题概述

对会话 `session-1786253044035-0`（im-bridge 全能力改造）的工具调用进行审计，发现约 100 次工具调用中存在**两类系统性问题**，导致修改代码需要反复写入调整：

| 问题 | 会话证据 | 失败次数 |
|---|---|---|
| **edit_file 对 CRLF 文件 100% 匹配失败** | `api_adapter.rs` 同一处修改连续失败 3 次（`old_string not found`），`main.rs`、`feishu_ws.rs` 各 1 次，最终全部被迫切换 `replace_lines` | 5 次失败 + 约 4 次额外定位调用 |
| **replace_lines 行号估算错误引发编译错误循环** | `feishu_ws.rs` 一处删 `.into()` 的 1 行改动，经历 4 次 replace_lines（3 次造成编译错误）才成功 | 3 次编译错误 + 5 次重复 read_file 验证 |
| **bash 命令引号转义错误无诊断** | grep 命令 `unexpected EOF while looking for matching '"'` | 1 次失败重试 |
| **bash timeout 参数单位歧义** | `timeout: 300` 被按 300ms 执行导致 clippy 超时 | 1 次失败重试 |

根因不是 AI 操作失误，而是**工具实现层面的缺陷**（详见 §4 代码事实）。

---

## 2. 目标与非目标

### 2.1 目标

1. 消除 Windows 平台下 edit_file 因 CRLF 行尾导致的批量匹配失败。
2. 让编辑类工具的返回结果携带**替换位置信息**，使 AI 在连续编辑时无需猜测行号。
3. 消除 bash 工具两个高频调用歧义（timeout 单位、命令语法错误提示）。

### 2.2 非目标

- 不重构 `replace_lines` 的匹配语义（行号式编辑保留，仅增强输出）。
- 不改变权限模型、沙箱行为。
- 不引入新的第三方依赖（如 shellcheck）。
- 不处理 AI 侧提示策略（prompt 层优化不在本 PRD 范围，可另立文档）。

---

## 3. 用户故事

- **US-1（Windows 开发者）**：我在 Windows 上让 AI 修改 CRLF 的 `.rs` 文件，AI 用 edit_file 一次成功，而不是反复收到 `old_string not found` 后被迫改用行号替换。
- **US-2（AI 编码助手）**：我连续编辑同一个文件时，edit_file / replace_lines 的返回结果告诉我"替换发生在第 X–Y 行"，我不再需要重新 read_file 来猜测后续操作的行号。
- **US-3（AI 编码助手）**：我调用 bash 传 `timeout: 300` 时，要么按秒解释，要么在 schema 中明确单位，我不再因为单位歧义触发 300ms 超时。
- **US-4（AI 编码助手）**：我的 bash 命令有引号转义错误时，工具错误信息直接指出"命令存在 shell 语法错误（引号/转义不匹配）"。

---

## 4. 代码事实（现状核查）

| 编号 | 位置 | 现状 |
|---|---|---|
| F1 | `rust/crates/runtime/src/file_ops.rs:512` | `edit_file` 用 `original_file.contains(old_string)` 直接子串匹配，**无行尾归一化** → CRLF 文件中 LF 的 old_string 匹配失败 |
| F2 | `rust/crates/runtime/src/file_ops.rs:616-645` | `write_file` 的旧版 edit 变体存在同样的匹配问题 |
| F3 | `rust/crates/runtime/src/file_ops.rs:715-719` | `replace_lines` **已做** CRLF 保留（检测原文件行尾风格），与 edit_file 形成不对称 |
| F4 | `rust/crates/runtime/src/file_ops.rs:526-535` | `EditFileOutput` 不含替换位置行号（只有 file_path/old_string/new_string/original_file/structured_patch） |
| F5 | `rust/crates/runtime/src/file_ops.rs:722-739` | `replace_lines` 行号越界已有清晰错误（含文件总行数） |
| F6 | `rust/crates/tools/src/lib.rs:880` | bash 工具 schema `"timeout": { "type": "integer", "minimum": 1 }` **无单位说明** |
| F7 | `rust/crates/tools/src/lib.rs:3012-3054` | `run_edit_file`/`run_replace_lines` 为薄封装，编辑后自动触发 `cargo check`（60s 超时，见 file_ops.rs:810） |

---

## 5. 需求规格

### P0-1　edit_file 行尾感知匹配（CRLF 兼容）

**动机**：会话中 5 次 `old_string not found` 全部源于此。`replace_lines`（F3）已有行尾保留逻辑，edit_file 缺失导致能力不对称。

**需求**：
- `edit_file` 在匹配前检测目标文件行尾风格（`\r\n` / `\n`），将 `old_string` 中的 `\n` 归一化为文件实际行尾后再执行 `contains` / `replace`。
- 写入后保持文件原有行尾风格（与 `replace_lines` 的 F3 行为一致），避免引入混合行尾。
- `write_file` 的旧版 edit 变体（F2）同步处理。

**验收标准**：
- 新增回归测试：CRLF 文件 + 纯 LF 的 `old_string` → 匹配成功，写入后文件仍为 CRLF。
- 新增回归测试：LF 文件 + LF `old_string` → 行为不变（无回归）。
- 现有 `edit_file` 测试（`tools/src/lib.rs:11613` 区域）全部通过。

**涉及文件**：`rust/crates/runtime/src/file_ops.rs`

---

### P0-2　编辑工具输出替换位置信息

**动机**：会话中 `feishu_ws.rs` 的 replace_lines 四连失败（3 次编译错误）根因是 AI 对行号/行数判断错误；`TodoWrite` 断言猜测输出结构也浪费 2 轮。工具返回的位置信息能让 AI 立即校准。

**需求**：
- `EditFileOutput` 增加字段：`start_line` / `end_line`（替换区间，1-based）、`affected_line_count`。
- `ReplaceLinesOutput` 增加字段：`replaced_line_count`、`new_total_lines`（替换后文件总行数）。
- 在 edit_file / replace_lines 的 `input_schema` description 中注明该输出字段，引导 LLM 使用。

**验收标准**：
- 对同一文件连续执行两次编辑：第二次基于第一次返回的 `end_line` 计算偏移，无需重新 read_file 即可定位。
- 工具返回的 JSON 中包含上述字段（`serde` 默认命名即可）。

**涉及文件**：`rust/crates/runtime/src/file_ops.rs`、`rust/crates/tools/src/lib.rs`

---

### P1-3　edit_file 匹配失败错误信息增强

**动机**：会话中 AI 连续 3 次以相同参数重试同一失败，说明错误信息 `old_string not found in file` 未给出可行动的线索。

**需求**：
- `edit_file` 匹配失败时：
  - 若检测到文件含 `\r\n` 而 `old_string` 不含 → 错误信息附加提示「文件为 CRLF 行尾，old_string 需匹配行尾或改用 replace_lines」。
  - 错误信息中附带文件总行数与文件行尾风格，辅助 AI 决策。
- P0-1 实施后该场景多数自动消失，本条作为兜底诊断。

**验收标准**：CRLF 文件 + LF old_string（在归一化未命中残余场景）返回的错误包含「CRLF」提示。

**涉及文件**：`rust/crates/runtime/src/file_ops.rs`

---

### P1-4　bash timeout 单位语义明确

**动机**：会话中 `timeout: 300` 被按 300ms 执行（F6 schema 无单位说明），导致 cargo clippy 超时中断。

**需求**：
- bash 工具 schema（F6）的 `timeout` 字段增加 `description`：明确单位为**毫秒**，并注明默认值。
- 实现层对 `timeout < 1000` 的极小值（如 < 1s）在日志中打 warning，提示可能的单位误用（不阻断执行）。

**验收标准**：schema 渲染出的 tool definition 包含单位说明；`timeout=300` 触发实现层 warning 日志。

**涉及文件**：`rust/crates/tools/src/lib.rs`、bash 执行层（`runtime`）

---

### P2-5　bash 命令语法错误诊断（低优先级）

**动机**：会话中 grep 命令因引号转义错误返回 `unexpected EOF while looking for matching '"'`，AI 需自行解读。

**需求**：
- bash 工具在 stderr 匹配 shell 语法错误特征（`unexpected EOF` / `syntax error near` 等）时，`returnCodeInterpretation` 输出「命令存在 shell 语法错误（引号/转义不匹配）」。

**验收标准**：构造含未闭合引号的命令，工具结果包含语法错误提示。

**涉及文件**：bash 执行层（`runtime`）

---

## 6. 非功能需求

| 维度 | 要求 |
|---|---|
| 向后兼容 | P0-2 新增输出字段为 **additive**，不破坏现有消费方；P0-1 匹配语义放宽，不存在破坏性 |
| 性能 | 行尾检测为 O(1) 扫描（读文件首行），无额外 IO |
| 测试 | 每条需求配套单元/回归测试；`cargo clippy --workspace --all-targets -- -D warnings` 保持通过 |
| 平台 | 重点回归 Windows（CRLF 场景）+ 非 Windows（LF 场景） |

---

## 7. 验收与测试计划

1. **CRLF 回归套件**（P0-1 / P1-3）：新建 `file_ops.rs` 测试模块，覆盖 CRLF/LF 双端矩阵（编辑、写入、混合行尾文件）。
2. **连续编辑场景**（P0-2）：集成测试模拟「同一文件两次 edit_file，第二次基于第一次返回值定位」。
3. **bash 诊断**（P1-4 / P2-5）：schema 断言 + 构造错误命令验证提示。
4. **全量回归**：`cargo test -p runtime -p tools` + `cargo clippy -p runtime -p tools --all-targets -- -D warnings`。
5. **真机验证**：在 Windows 下用 claw 对 CRLF 文件执行 edit_file，确认一次成功（复现会话中 `api_adapter.rs` 场景）。

---

## 8. 风险与开放问题

| 风险 | 说明 | 对策 |
|---|---|---|
| 匹配语义放宽可能误替换 | P0-1 归一化后 `old_string` 仅含 `\n` 时可能命中多处 | 沿用 `replacen(…, 1)` 仅替换首个；`replace_all` 显式开启才全替 |
| 输出字段变更影响 TUI/CLI 渲染 | `EditFileOutput` 新增字段为 additive，渲染层忽略未知字段 | 合并前跑 `rusty-claude-cli` 冒烟 |
| timeout warning 误报 | `< 1000ms` 的合法场景（快速命令）会触发 warning | 仅日志不阻断，且注明「疑似单位误用」 |

**开放问题**：
- P0-2 的字段命名是否需要遵循既有 `structured_patch` 风格（如 `git_diff` 为 camelCase）？建议合并时与 `serde` 现状对齐即可，不做额外 rename。

---

## 9. 附录：会话证据索引

| 证据 | 事件 | 位置（会话 JSONL 行号） |
|---|---|---|
| E1 | `api_adapter.rs` edit_file 连续 3 次 `old_string not found`，切 replace_lines 成功 | L94 / L96 / L102 / L104 |
| E2 | `main.rs` edit_file 失败，切 replace_lines | L171 / L174 |
| E3 | `feishu_ws.rs` replace_lines 4 次往返、3 次编译错误 | L196 / L200 / L204 / L208 |
| E4 | bash 引号转义错误 `unexpected EOF` | L34 |
| E5 | bash `timeout: 300` 按 300ms 超时 | L112 / L114 |
| E6 | TodoWrite 输出结构猜测 2 次（status→new_todos→newTodos） | L135 / L140 / L146 |
