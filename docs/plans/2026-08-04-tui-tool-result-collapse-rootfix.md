# TUI 工具结果折叠根治实施计划

**Goal:** 根治 TUI 中"输出内容被当成工具输出折叠"——折叠不需要用户看到的工具信息，展开需要用户看到的输出内容。

**Architecture:** 三层联动：(1) 在 `tui/tool_card.rs` 新增内容提取层 `extract_tool_output_body`，把 pretty JSON 信封还原为真实内容（stdout/file.content/matches/result）；(2) 重写 `tui/output_view.rs` 的 `compute_priority` 兜底启发式为语义分类器（read_file/grep/测试/错误默认展开，write/edit/glob 折叠）；(3) 渲染层 `render_tool_result` 的行数/语言检测/正文全部基于提取内容，展开视图 60 行截断（P0 截尾部，其余截头部）。

**Tech Stack:** Rust, ratatui, serde_json, rusty-claude-cli crate（`full-tui` feature）。

**前置事实（已核查，勿重复调查）：**

| 工具 | 输出 JSON 字段路径 | 验证位置 |
|------|-------------------|----------|
| bash | `stdout` / `stderr`（snake_case 无 rename） | `runtime/src/bash.rs:70-71` |
| read_file | `file.content`（`filePath`/`numLines` camelCase rename） | `runtime/src/file_ops.rs:285-289` |
| write_file | `content` + `structuredPatch` + `gitDiff`；**可能追加 `\n\n--- cargo check ---\n{输出}`** | `tools/src/lib.rs:2957-2960` |
| edit_file | `structuredPatch` + `gitDiff`；**同样追加 cargo check** | `tools/src/lib.rs:2979-2982` |
| glob_search | `filenames: Vec<String>` | `runtime/src/file_ops.rs:349` |
| grep_search | `content: Option<String>`（预格式化匹配文本） | `runtime/src/file_ops.rs:386-390` |
| WebFetch | `result` | `tools/src/lib.rs:3813` |
| WebSearch | `results`（枚举变体）→ **回退原始输出** | `tools/src/lib.rs:4026` |

**工作区警告：** 当前 git 工作区已有与本任务无关的未提交改动（`runtime/src/conversation.rs`、`planner/artifact.rs`、`tui/app.rs`、`tui/output_view.rs` 的 plan 缓存修复）。**只 `git add` 本任务涉及的文件，严禁 `git add -A`。**

---

## File Structure

- `rust/crates/rusty-claude-cli/src/tui/tool_card.rs` — 新增 `extract_tool_output_body`（提取层）+ `extract_tool_output_body_public`（供 output_view 调用）+ `MAX_EXPANDED_LINES` 常量；修改 `render_tool_result`（接线提取 + 截断）。约 +120 行。
- `rust/crates/rusty-claude-cli/src/tui/output_view.rs` — 新增 `BASH_ERROR_MARKERS` 常量；重写 `compute_priority` 第 4 步为语义分类器。约 +60 行。
- 测试写在两文件各自的 `#[cfg(test)] mod tests` 内（沿用现有内联测试模块模式）。

---

### Task 1: 内容提取层 `extract_tool_output_body`

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/tool_card.rs`（新增函数，放在 `render_tool_result` 之后、`detect_language_for_tool` 之前）
- Test: 同文件 `#[cfg(test)] mod tests`（追加 6 个测试）

- [ ] **Step 1: 写失败测试（追加到 tool_card.rs 测试模块末尾，`render_timeline_empty_history` 之后）**

```rust
    // ---------- 提取层测试（P0 修复 2026-08-04） ----------

    /// 提取：bash pretty JSON 信封 → stdout（stderr 为空时不追加）
    #[test]
    fn extract_tool_output_body_bash_extracts_stdout() {
        let output = r#"{
  "stdout": "hello\nworld",
  "stderr": "",
  "returnCodeInterpretation": "exit_code:0",
  "sandboxStatus": {}
}"#;
        let body = extract_tool_output_body("bash", output);
        assert_eq!(body, "hello\nworld");
    }

    /// 提取：bash stderr 非空时追加（错误常在 stderr，不能丢）
    #[test]
    fn extract_tool_output_body_bash_appends_stderr() {
        let output = r#"{
  "stdout": "partial output",
  "stderr": "boom\nproblem",
  "returnCodeInterpretation": "exit_code:1"
}"#;
        let body = extract_tool_output_body("bash", output);
        assert_eq!(body, "partial output\n[stderr]\nboom\nproblem");
    }

    /// 提取：read_file → file.content
    #[test]
    fn extract_tool_output_body_read_file_extracts_content() {
        let output = r#"{
  "type": "file",
  "file": {
    "filePath": "src/main.rs",
    "content": "fn main() {}\n",
    "numLines": 1,
    "startLine": 1,
    "totalLines": 1
  }
}"#;
        let body = extract_tool_output_body("read_file", output);
        assert_eq!(body, "fn main() {}\n");
    }

    /// 提取：write_file 带 cargo check 尾部 → 提取 cargo check 错误，剔除确认 JSON
    #[test]
    fn extract_tool_output_body_write_file_with_cargo_check() {
        let output = format!(
            "{}\n\n--- cargo check ---\nerror[E0308]: mismatched types\n --> src/main.rs:2:23",
            r#"{
  "type": "write",
  "filePath": "src/main.rs",
  "content": "fn main() {}",
  "structuredPatch": [],
  "originalFile": null,
  "gitDiff": null
}"#
        );
        let body = extract_tool_output_body("write_file", &output);
        assert!(body.contains("error[E0308]"), "应提取 cargo check 错误: {body}");
        assert!(!body.contains("filePath"), "不应包含确认 JSON: {body}");
    }

    /// 提取：非 JSON 结果回退原始输出（不崩溃）
    #[test]
    fn extract_tool_output_body_non_json_falls_back() {
        let body = extract_tool_output_body("bash", "plain text output");
        assert_eq!(body, "plain text output");
    }

    /// 提取：未知工具回退原始输出
    #[test]
    fn extract_tool_output_body_unknown_tool_falls_back() {
        let body = extract_tool_output_body("WebSearch", r#"{"query":"x"}"#);
        assert_eq!(body, r#"{"query":"x"}"#);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p rusty-claude-cli --lib -- tui::tool_card::extract_tool_output_body 2>&1 | tail -15`
Expected: `error[E0425]: cannot find function 'extract_tool_output_body' in this scope`（函数不存在）

- [ ] **Step 3: 实现提取函数（插入在 `render_tool_result` 函数结束之后、`detect_language_for_tool` 之前）**

```rust
/// 从工具结果 JSON 信封中提取面向用户的真实内容。
///
/// 背景（P0 修复 2026-08-04）：所有工具结果经 `serde_json::to_string_pretty`
/// 序列化为 pretty JSON 信封（bash 17 字段 + sandboxStatus，3 行 stdout 会膨胀
/// 成 38 行 JSON）。折叠判定与渲染此前直接统计/展示信封，导致"输出内容被当成
/// 工具输出折叠"。本函数按工具结构提取真实内容；未知工具/非 JSON 回退原始输出。
///
/// 字段路径与 `src/tool_display.rs` 的 format_*_result 保持一致（同一份 schema）。
fn extract_tool_output_body(name: &str, output: &str) -> String {
    use serde_json::Value;
    match name {
        // bash：stdout 是主体；stderr 非空时追加（错误常在 stderr）
        "bash" | "Bash" => match serde_json::from_str::<Value>(output) {
            Ok(v) => {
                let stdout = v.get("stdout").and_then(|s| s.as_str()).unwrap_or("");
                let stderr = v.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
                if stderr.is_empty() {
                    stdout.to_string()
                } else {
                    format!("{stdout}\n[stderr]\n{stderr}")
                }
            }
            Err(_) => output.to_string(),
        },
        // read_file：内容是答案
        "read_file" | "Read" => match serde_json::from_str::<Value>(output) {
            Ok(v) => v
                .get("file")
                .and_then(|f| f.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => output.to_string(),
        },
        // grep_search：content 是预格式化匹配文本
        "grep_search" | "Grep" => match serde_json::from_str::<Value>(output) {
            Ok(v) => v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => output.to_string(),
        },
        // glob_search：文件名列表（分类器给 P3，这里提取以备不时之需）
        "glob_search" | "Glob" => match serde_json::from_str::<Value>(output) {
            Ok(v) => v
                .get("filenames")
                .and_then(|f| f.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            Err(_) => output.to_string(),
        },
        // WebFetch：result 是正文
        "WebFetch" => match serde_json::from_str::<Value>(output) {
            Ok(v) => v
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => output.to_string(),
        },
        // write/edit：纯确认 JSON → 空；若尾部带 cargo check 输出（JSON 解析
        // 失败），提取 cargo check 文本——编译错误是信号，不能折叠掉。
        "write_file" | "Write" | "edit_file" | "Edit" => {
            match serde_json::from_str::<Value>(output) {
                Ok(_) => String::new(),
                Err(_) => output
                    .split_once("--- cargo check ---")
                    .map(|(_, tail)| tail.trim_start().to_string())
                    .unwrap_or_default(),
            }
        }
        // WebSearch/未知工具：结构异构，回退原始输出
        _ => output.to_string(),
    }
}

/// 公开接口：供 `output_view.rs` 的 compute_priority 调用（沿用 P1 重构的
/// `_public` 包装模式，见 render_tool_result_public）。
pub(crate) fn extract_tool_output_body_public(name: &str, output: &str) -> String {
    extract_tool_output_body(name, output)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p rusty-claude-cli --lib -- tui::tool_card::extract_tool_output_body 2>&1 | tail -15`
Expected: `test result: ok. 6 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/tui/tool_card.rs
git commit -m "feat(tui): 工具结果内容提取层 - 摆脱 pretty JSON 信封（stdout/file.content/cargo check）"
```

---

### Task 2: 语义分类器重写 `compute_priority`

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/output_view.rs`
- Test: 同文件 `#[cfg(test)] mod tests`（追加 5 个测试）

- [ ] **Step 1: 写失败测试（追加到 output_view.rs 测试模块，`compute_priority_emphasis_normal` 之后）**

```rust
    // ---------- 语义分类器测试（P0 修复 2026-08-04） ----------

    /// 核心回归：bash 3 行 stdout 的 pretty JSON 信封（18 行）应展开（P1）。
    /// 旧实现统计信封行数 → 恒 P2 折叠；新实现提取 stdout（3 行 ≤ 8）→ P1。
    #[test]
    fn compute_priority_pretty_json_short_stdout_expands() {
        let input = r#"{"command":"echo hi"}"#;
        let result = r#"{
  "stdout": "line1\nline2\nline3",
  "stderr": "",
  "interrupted": false,
  "isImage": false,
  "backgroundTaskId": null,
  "backgroundedByUser": false,
  "assistantAutoBackgrounded": false,
  "dangerouslyDisableSandbox": false,
  "returnCodeInterpretation": "exit_code:0",
  "noOutputExpected": false,
  "structuredContent": null,
  "persistedOutputPath": null,
  "persistedOutputSize": null,
  "sandboxStatus": {
    "enabled": true,
    "supported": true,
    "active": false
  }
}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P1);
    }

    /// bash stdout 含错误标记（rc 为 0 时也能命中）→ P0，内容信号覆盖行数
    #[test]
    fn compute_priority_bash_error_marker_is_p0() {
        let input = r#"{"command":"cargo build"}"#;
        let result = r#"{
  "stdout": "error: could not compile `demo`",
  "returnCodeInterpretation": "exit_code:0"
}"#;
        assert_eq!(compute_priority("bash", input, result, false), Priority::P0);
    }

    /// bash 长输出含 test result:（41 行全过测试）→ P1 展开（测试总结是信号）
    #[test]
    fn compute_priority_bash_test_result_long_output_expands() {
        let input = r#"{"command":"cargo test"}"#;
        let mut stdout = String::new();
        for i in 0..40 {
            stdout.push_str(&format!("test tests::case{i} ... ok\n"));
        }
        stdout.push_str("test result: ok. 40 passed");
        let result = format!(
            "{{\n  \"stdout\": \"{}\",\n  \"returnCodeInterpretation\": \"exit_code:0\"\n}}",
            stdout.replace('\n', "\\n")
        );
        assert_eq!(compute_priority("bash", input, &result, false), Priority::P1);
    }

    /// read_file 20 行内容（信封 10 行）→ P1 展开（内容是答案，门槛放宽到 40）
    #[test]
    fn compute_priority_read_file_20_lines_expands() {
        let input = r#"{"path":"src/main.rs"}"#;
        let content = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let result = format!(
            "{{\n  \"type\": \"file\",\n  \"file\": {{\n    \"filePath\": \"src/main.rs\",\n    \"content\": \"{content}\",\n    \"numLines\": 20,\n    \"startLine\": 1,\n    \"totalLines\": 20\n  }}\n}}"
        );
        assert_eq!(compute_priority("read_file", input, &result, false), Priority::P1);
    }

    /// write_file 纯确认 → P3 单行摘要（过程噪音折叠）
    #[test]
    fn compute_priority_write_file_confirms_p3() {
        let input = r#"{"path":"a.txt","content":"hi"}"#;
        let result = r#"{
  "type": "write",
  "filePath": "a.txt",
  "content": "hi",
  "structuredPatch": [],
  "originalFile": null,
  "gitDiff": null
}"#;
        assert_eq!(compute_priority("write_file", input, result, false), Priority::P3);
    }

    /// write_file 带 cargo check 编译错误 → P0（错误是信号）
    #[test]
    fn compute_priority_write_file_cargo_check_error_p0() {
        let input = r#"{"path":"src/main.rs","content":"fn main() {}"}"#;
        let result = format!(
            "{}\n\n--- cargo check ---\nerror[E0308]: mismatched types\n --> src/main.rs:2:23",
            r#"{
  "type": "write",
  "filePath": "src/main.rs",
  "content": "fn main() {}",
  "structuredPatch": [],
  "originalFile": null,
  "gitDiff": null
}"#
        );
        assert_eq!(compute_priority("write_file", input, &result, false), Priority::P0);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p rusty-claude-cli --lib -- tui::output_view::compute_priority 2>&1 | tail -25`
Expected: 6 个新测试 FAIL（旧实现信封行数兜底）；例如 `compute_priority_pretty_json_short_stdout_expands ... FAILED`（旧代码 18 行信封 → P2 ≠ P1）

- [ ] **Step 3: 实现分类器（重写 `compute_priority` 第 4 步，并在函数上方新增常量）**

在 `compute_priority` 函数前新增常量：

```rust
/// bash/编辑结果中的错误标记，命中即 P0 展开（内容信号覆盖行数）。
/// 大小写分别收录："error:"(Rust/cargo/shell) 与 "Error:"(Node/Python)。
const BASH_ERROR_MARKERS: &[&str] = &[
    "error[E",
    "error:",
    "Error:",
    "panic!",
    "fatal:",
    "FAILED",
    "command not found",
    "Traceback",
];
```

将 `compute_priority` 第 4 步（从 `// 4. 行数启发式兜底` 到函数结尾的 `}`）整体替换为：

```rust
    // 4. 内容语义分类器兜底（P0 修复 2026-08-04）
    // 根因：旧实现统计 pretty JSON 信封行数（3 行 stdout → 38 行 JSON 恒 P2
    // 折叠），且与内容语义无关。现在先提取真实内容，再按工具语义决定默认
    // 展开层级：内容是答案（read_file/grep/测试/错误）→ 展开；
    // 过程噪音（write/edit/glob）→ 折叠单行。
    let body = crate::tui::tool_card::extract_tool_output_body_public(tool_name, result);
    let lines = body.lines().count();
    match tool_name {
        "bash" | "Bash" => {
            if BASH_ERROR_MARKERS.iter().any(|m| body.contains(m)) {
                Priority::P0 // 命令失败/编译错误：内容信号覆盖行数
            } else if body.contains("test result:") {
                Priority::P1 // 测试总结（cargo test 末行）——长输出也展开
            } else if lines > 8 {
                Priority::P2 // 长输出折叠（ls -la 等过程输出）
            } else {
                Priority::P1
            }
        }
        "read_file" | "Read" => {
            // 内容是答案，门槛放宽到 40 行
            if lines > 40 {
                Priority::P2
            } else {
                Priority::P1
            }
        }
        "grep_search" | "Grep" => {
            // 命中即证据，门槛放宽到 50 行
            if lines > 50 {
                Priority::P2
            } else {
                Priority::P1
            }
        }
        "edit_file" | "Edit" | "write_file" | "Write" => {
            if BASH_ERROR_MARKERS.iter().any(|m| body.contains(m)) {
                Priority::P0 // cargo check 编译错误 → 展开显示错误
            } else {
                Priority::P3 // 纯确认 → 单行摘要
            }
        }
        "glob_search" | "Glob" | "Skill" | "TodoWrite" | "ToolSearch"
        | "benchmark_compare" => {
            Priority::P3 // 过程噪音/确认：单行摘要
        }
        "WebFetch" => {
            if lines > 8 {
                Priority::P2
            } else {
                Priority::P1
            }
        }
        _ => {
            if lines > 8 {
                Priority::P2
            } else {
                Priority::P1
            }
        }
    }
```

- [ ] **Step 4: 运行全量 compute_priority 测试确认通过（含旧测试回归）**

Run: `cargo test -p rusty-claude-cli --lib -- tui::output_view::compute_priority 2>&1 | tail -25`
Expected: `test result: ok. 15 passed; 0 failed`（6 新 + 9 旧）

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/tui/output_view.rs
git commit -m "feat(tui): compute_priority 语义分类器 - read_file/grep/测试/错误展开，write/edit/glob 折叠"
```

---

### Task 3: 渲染层接线 + 展开 60 行截断

**Files:**
- Modify: `rust/crates/rusty-claude-cli/src/tui/tool_card.rs`
- Test: 同文件 `#[cfg(test)] mod tests`（追加 3 个测试）

- [ ] **Step 1: 写失败测试（追加到 tool_card.rs 测试模块，Task 1 的 6 个测试之后）**

```rust
    // ---------- 渲染接线测试（P0 修复 2026-08-04） ----------

    /// 核心回归：展开卡片渲染真实 stdout，而非 JSON 信封（无 sandboxStatus/键名）
    #[test]
    fn render_tool_result_renders_body_not_json_envelope() {
        let output = r#"{
  "stdout": "hello",
  "stderr": "",
  "sandboxStatus": { "enabled": true }
}"#;
        let card = render_tool_result("bash", output, false, None, false, Priority::P1);
        assert!(card.contains("hello"), "应渲染 stdout 内容: {card}");
        assert!(!card.contains("sandboxStatus"), "不应渲染 JSON 信封: {card}");
        assert!(!card.contains("\"stdout\""), "不应渲染 JSON 键: {card}");
    }

    /// 展开 + 截断：100 行 body → 显示前 60 行 + 省略标记（P1 截头部）
    #[test]
    fn render_tool_result_expanded_truncates_head_at_60() {
        let output = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = render_tool_result("bash", &output, false, None, false, Priority::P1);
        assert!(card.contains("line1"), "应显示前 60 行: {card}");
        assert!(card.contains("line60"), "应显示到第 60 行: {card}");
        assert!(!card.contains("line61"), "不应显示第 61 行: {card}");
        assert!(card.contains("其余 40 行省略"), "应显示省略标记: {card}");
    }

    /// 错误展开截尾部：P0 显示最后 60 行（失败摘要/错误列表常在输出末尾）
    #[test]
    fn render_tool_result_error_truncates_tail_at_60() {
        let output = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let card = render_tool_result("bash", &output, true, None, true, Priority::P0);
        assert!(card.contains("line100"), "P0 应显示最后一行: {card}");
        assert!(card.contains("line41"), "P0 应显示尾部内容: {card}");
        assert!(!card.contains("line1"), "P0 不应显示开头: {card}");
        assert!(card.contains("其余 40 行省略"), "应显示省略标记: {card}");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p rusty-claude-cli --lib -- tui::tool_card::render_tool_result_renders 2>&1 | tail -15`
Expected: FAILED（旧实现渲染 JSON 信封，含 `sandboxStatus`；且无截断）

- [ ] **Step 3: 实现接线与截断**

3a. 新增常量（放在 `COLLAPSE_THRESHOLD` 定义附近，第 12 行）：

```rust
/// 展开视图最大行数（P0 修复 2026-08-04：展开+截断，兼顾可见性与空间）。
/// P1 截头部；P0（错误）截尾部——错误摘要/失败列表常在输出末尾。
const MAX_EXPANDED_LINES: usize = 60;
```

3b. `render_tool_result` 内，把第 92 行 `let line_count = output.lines().count();` 替换为：

```rust
    // P0 修复(2026-08-04):行数与正文基于提取后的真实内容,而非 pretty JSON 信封。
    // 信封行数会导致"3 行 stdout 显示 38 行 JSON、恒被折叠"。
    let body = extract_tool_output_body(name, output);
    let line_count = body.lines().count();
```

3c. 把第 119 行 `let language = detect_language_for_tool(name, output);` 替换为：

```rust
    let language = detect_language_for_tool(name, &body);
```

3d. 把完整视图分支（`} else {` 之后的完整视图，约第 155-160 行）替换为：

```rust
    } else {
        // 完整视图：基于提取后的真实内容；超 60 行截断（展开+截断）。
        // P0(错误)截尾部——失败摘要常在末尾；其余截头部。
        let lines: Vec<&str> = body.lines().collect();
        let total = lines.len();
        if total > MAX_EXPANDED_LINES {
            let (shown, hidden) = if priority == Priority::P0 {
                (&lines[total - MAX_EXPANDED_LINES..], total - MAX_EXPANDED_LINES)
            } else {
                (&lines[..MAX_EXPANDED_LINES], total - MAX_EXPANDED_LINES)
            };
            let body_str = highlighted_body(shown);
            format!(
                "{diff_prefix}├─ {icon} {name} ({total} 行，截断)\n{body_str}│ …（其余 {hidden} 行省略）\n└─\n"
            )
        } else {
            let body_str = highlighted_body(&lines);
            format!("{diff_prefix}├─ {icon} {name} ({total} 行)\n{body_str}└─\n")
        }
    }
```

> 说明：P3 分支保持调用 `summarize_tool_result(name, inp, output, is_error)`（它需要原始 JSON 的字段）；折叠单行标题分支不变；line_count==0 的 `(空)` 分支不变。`render_tool_result` 下方有个同条件死代码折叠预览分支（2026-08-01 已改为单行标题后遗留），**不要动它**，与本任务无关。

- [ ] **Step 4: 运行全量 tui::tool_card 测试确认通过（含旧测试回归）**

Run: `cargo test -p rusty-claude-cli --lib -- tui::tool_card 2>&1 | tail -15`
Expected: `test result: ok. 23 passed; 0 failed`（3 新 + 20 旧）

- [ ] **Step 5: Commit**

```bash
git add rust/crates/rusty-claude-cli/src/tui/tool_card.rs
git commit -m "feat(tui): 渲染接线提取内容 + 展开 60 行截断（P0 截尾部/P1 截头部）"
```

---

### Task 4: 全量回归 + fmt + clippy

**Files:** 无新改动（验证 + 修复 lint）

- [ ] **Step 1: 运行 tui 全量测试**

Run: `cargo test -p rusty-claude-cli --lib -- tui:: 2>&1 | tail -20`
Expected: `test result: ok. 76 passed; 0 failed`（基线 47 + 新增 15 + 既有 tui 模块其他测试）

- [ ] **Step 2: 运行格式化检查**

Run: `scripts/fmt.sh --check 2>&1 | tail -10`
若失败：Run: `scripts/fmt.sh`（只格式化，随后重新运行 Step 1 确认测试仍通过）

- [ ] **Step 3: 运行 clippy**

Run: `cd rust && cargo clippy -p rusty-claude-cli --all-targets -- -D warnings 2>&1 | tail -10`
Expected: 无 warning 输出（退出码 0）。若报 lint，修复后重跑 Step 1。

- [ ] **Step 4: 确认工作区干净（仅本任务文件被提交）**

Run: `git status --short`
Expected: 只剩与本次任务无关的既有改动（`runtime/src/conversation.rs`、`planner/artifact.rs`、`tui/app.rs`、`tui/output_view.rs`）——即 Task 2 修改过 output_view.rs 属正常；不应有其他新文件游离。

- [ ] **Step 5: Commit（若 Step 2/3 产生修复）**

```bash
git add rust/crates/rusty-claude-cli/src/tui/tool_card.rs rust/crates/rusty-claude-cli/src/tui/output_view.rs
git commit -m "chore(tui): fmt/clippy 修复"
```

---

## Self-Review 记录

### 文档正确性检查

**1. Spec coverage（设计文档 §3.1-3.4 → 计划任务）：**
- §3.1 提取层 → Task 1 ✅
- §3.2 语义分类器 → Task 2 ✅
- §3.3 渲染接线 + 截断 → Task 3 ✅
- §3.4 交互不变（Tab 折叠、J/K、trim）→ 不涉及代码改动，回归覆盖 ✅
- 测试计划 §4.1 全部 9 项 → Task 1(3项) + Task 2(5项) + Task 3(3项) = 11 项（超量覆盖）✅

**2. Placeholder scan:** 无 TBD/TODO；每个代码步骤含完整可粘贴代码。✅

**3. Type consistency:**
- `extract_tool_output_body(name: &str, output: &str) -> String` — Task 1 定义，Task 2 经 `extract_tool_output_body_public` 调用（同名同签名的 pub(crate) 包装）、Task 3 内部调用 ✅
- `MAX_EXPANDED_LINES: usize = 60` — Task 3a 定义，Task 3d 使用 ✅
- `BASH_ERROR_MARKERS: &[&str]` — Task 2 定义（output_view.rs），Task 2 分类器使用 ✅
- `highlighted_body(&[&str])` 闭包签名与切片 `&lines[..MAX]` / `&lines[total-MAX..]` 兼容 ✅

### 代码事实核查（必做，全部已实际验证）

| # | 声明 | 验证 |
|---|------|------|
| 4a | bash 输出字段 `stdout`/`stderr` snake_case | ✅ `runtime/src/bash.rs:70-71`（grep 已读） |
| 4b | read_file 信封 `{"type":"file","file":{"content":...}}` | ✅ `runtime/src/file_ops.rs:285-289`（read 已读） |
| 4c | write/edit 追加 `--- cargo check ---` | ✅ `tools/src/lib.rs:2960,2981`（grep 已读） |
| 4d | grep `content` 字段存在 | ✅ `runtime/src/file_ops.rs:386`（grep 已读） |
| 4e | glob `filenames: Vec<String>` | ✅ `runtime/src/file_ops.rs:349`（grep 已读） |
| 4f | WebFetch `result` | ✅ `tools/src/lib.rs:3813`（read 已读） |
| 4g | `compute_priority(tool_name, input, result, is_error)` 签名 + 前 3 步（emphasis/is_error/rc） | ✅ `output_view.rs:59-98`（read 已读） |
| 4h | `render_tool_result` 结构（diff_prefix→line_count→P3→effective_collapsed→折叠单行→highlight→完整视图） | ✅ `tool_card.rs:73-161`（read 已读） |
| 4i | 第 92 行 `line_count = output.lines().count()`、第 119 行 `detect_language_for_tool(name, output)` 位置 | ✅ grep 行号 + read 对齐 |
| 4j | output_view.rs 用 `crate::tui::tool_card::xxx_public` 全限定路径 | ✅ `output_view.rs:204,217`（grep 已读） |
| 4k | 既有 47 测试全部与新模式兼容（逐条推演见下） | ✅ 见 Task 2/3 Step 4 预期 |

**既有测试兼容性逐条推演：**
- `compute_priority_bash_ok_short`（compact JSON, stdout 2 行）→ 提取 2 行 ≤ 8 → P1 ✅
- `compute_priority_long_output`（raw 50 行）→ 提取回退 50 行 > 8 → P2 ✅
- `compute_priority_9_lines_collapses` / `8_lines_expands`（raw）→ 回退门槛不变 ✅
- `compute_priority_bash_interrupted/exit_nonzero/idle_timeout`（rc 字段）→ 第 3 步先返回 ✅
- `compute_priority_emphasis_*` / `is_error` → 第 1/2 步先返回 ✅
- `render_tool_result_short_output_full_view`（raw 3 行）→ 回退 3 行 → 完整视图 ✅
- `render_tool_result_long_output_collapsed/expanded`（raw 20 行）→ 回退；20 ≤ 60 无截断 ✅
- `render_tool_result_p0_never_collapsed`（raw 50 行, P0）→ 50 ≤ 60 无截断，含 line50 ✅
- `render_tool_result_p3_shows_l1_summary` → P3 分支不变（用原始 output）✅
- `render_edit_diff_*`（edit_file, output="ok"）→ 提取规则：parse "ok" 失败 → split_once None → "" → line_count 0 → diff_prefix + `(空)`；断言只查 diff 颜色/内容 ✅
- `render_tool_result_empty_output`（""）→ 回退 "" → 0 行 → `(空)` ✅
- `complete_tool_card_sets_result`（raw "file1\nfile2"）→ 2 行 → P1 展开 ✅
- `complete_tool_card_long_output_shows_collapse_preview`（raw 50 行）→ P2 折叠单行 `50 行…折叠` ✅
- `toggle_expand_long_tool_card_shows_full_output`（raw 20 行, P2→toggle）→ 展开完整 20 行 ✅

### 实现可行性推演（9 项）

**7. 签名兼容：** `extract_tool_output_body_public(&str, &str) -> String` 是同步纯函数，`compute_priority` 参数齐备（tool_name/result 均为 `&str`），无 async/生命周期问题 ✅
**8. 参数来源：** `name`/`output` 来自 `complete_tool_card` → `OutputEntry` → `render()` 调用链，均已有 ✅
**9. 数据传递链：** result 从 SSE 事件 → `complete_tool_card(tool_id, result, is_error)` → `compute_priority(name, input, result, is_error)` + `OutputEntry::render` → `render_tool_result_public(name, output, ...)`，无断层 ✅
**10. 判定优先级：** emphasis > is_error > rc > 错误标记 > test result > 工具语义 > 行数。误判成本分析：`FAILED` 大写匹配 ls 输出含 "FAILED.txt" 的罕见情况 → P0 展开，成本低；漏判（错误被折叠）成本高 → 优先级顺序正确 ✅
**11. retry/重入：** 提取在每次 render/priority 计算时执行一次；`cached_lines` 增量缓存挡住无关重渲染，MB 级结果解析 ~ms 级 ✅
**12. 冲突处理：** 模型 emphasis=low 与结果含错误标记冲突 → emphasis 优先（模型明确意图），尊重模型 ✅
**13. 与现有系统重叠：** `summarize_tool_result`（P3 L1 摘要）与 `tool_display.rs` 各自解析 JSON，与提取层字段路径一致但输出不同（摘要 vs 正文），职责不重叠；不共享代码（YAGNI）✅
**14. 失败路径：** JSON 解析失败 → 回退原始；缺字段 → 空串 → `(空)`；write/edit 无 cargo check 标记 → ""；未知工具 → 原始 + 8 行门槛。全部降级不崩溃 ✅
**15. 构造点破坏扫描：** 零新增结构体字段/枚举变体（只加函数+常量），无 `..Default::default()` 构造点受影响 ✅
**16. 成本估算：** 提取层 105 行 + 分类器 55 行 + 渲染接线 25 行 + 测试 210 行 ≈ 395 行 + 设计/计划文档。含错误处理（回退分支）与边界 case（cargo check 尾部、stderr 追加、截断方向）✅

### 计划中发现并内联修复的问题

1. **write/edit cargo check 尾部**（核查时发现 `tools/lib.rs:2960,2981` 追加 `--- cargo check ---`）：若提取为空会把编译错误折叠掉 → 提取层提取 cargo check 文本 + 分类器对含错误标记的 write/edit 给 P0（Task 1/2 已含测试）。
2. **P0 截断方向**：`cargo test` 失败摘要/`error:` 块常在输出末尾，P0 截尾部而非头部（Task 3 测试 `render_tool_result_error_truncates_tail_at_60` 固化）。
3. **`FAILED` 大小写**：`ls` 输出小写 "failed" 不误伤，cargo test 大写 `FAILED` 必命中 → 标记列表用大写 `FAILED`（设计文档已注）。
