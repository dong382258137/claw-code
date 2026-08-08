# 助手重复诊断死循环 —— 根本性修复实施计划（P0–P2）

**Goal:** 从根因上消除"助手重复同一组诊断动作导致会话卡死"：让"已尝试且失败"成为系统自动记录的事实（预防），让循环检测跨 turn 生效且能真正终止 turn（检测），让终止路径产出可诊断信息（兜底）。

**Architecture:** 三层防线对应根因链。P0：`history_search` 增加 `content_raw` 列（检索结果不再被 CJK 切分空格污染）+ 运行时在工具失败路径自动追加 NOTEBOOK `<attempted>` 段（不依赖 LLM 主动记账）。P1：`LoopDetector` 工具调用计数从"每 turn 全量 reset"改为"时间窗口衰减"（跨 turn 生效）+ 输出规范化比对 + `Abort` 从"仅标错"升级为"真正终止 turn"。P2：迭代上限与 loop abort 的错误消息携带诊断上下文（已尝试记录在 `<attempted>` 段），供下一 turn 改变策略。

**Tech Stack:** Rust（runtime crate）、rusqlite 0.31（bundled SQLite 3.45，FTS5）、SQLite FTS5 `unicode61` tokenizer（保持现方案，不引入 trigram 双轨）。

---

## 一、现状分析（代码事实核查）

> 以下位置均已通过 Read/Grep 实际验证。验证标记：✅ = 已核实。

| 组件 | 位置 | 现状 | 验证 |
|------|------|------|------|
| history 表 schema | `history_search.rs:60-70` | `content` 存切分文本，无 `content_raw` 列 | ✅ |
| `index_message` | `history_search.rs:92-113` | content 列 = `tokenize_content_for_index(content)`（污染源） | ✅ |
| `search` | `history_search.rs:126-160` | `SELECT content...` 返回切分文本 | ✅ |
| `migrate_from_v1` | `history_search.rs:347-390` | 重建表存切分文本，无 content_raw | ✅ |
| `HistoryHit` | `history_search.rs:401-413` | `content` 字段（被污染） | ✅ |
| 写透历史索引 | `session.rs:805-840` | `append_persisted_message` 调 `index_message(content=原文)` | ✅ |
| 检索消费点 | `conversation.rs:3152-3200` | `execute_session_search` snippet = `hit.content`（污染） | ✅ |
| `LoopDetector` 结构 | `loop_detection.rs:63-86` | 3 组计数 HashMap + 1 个 HashSet | ✅ |
| `record_tool_call` | `loop_detection.rs:150-214` | 同输入（3/6）+ 同输出（5/10）双通道 | ✅ |
| `reset` | `loop_detection.rs:216-223` | 全量清空 | ✅ |
| `normalize_tool_input` | `loop_detection.rs:250-258` | JSON/空白规范化；**输出无规范化** | ✅ |
| 每 turn 全量 reset | `conversation.rs:1987` | `self.loop_detector.reset()`（跨 turn 循环不可见） | ✅ |
| loop 接入成功路径 | `conversation.rs:1848-1950` | `Abort` → `cancelled_with_message` 仅把工具结果标错，turn 继续 | ✅ |
| loop **未**接入失败路径 | `conversation.rs:1952-1973` | 失败工具调用完全不走循环检测 | ✅ |
| `max_iterations` | `conversation.rs:769, 2084` | `DEFAULT_MAX_ITERATIONS = 64`；超限裸 `record_turn_failed` + `return Err`（未走 `try_recover_or_record_fail`） | ✅ |
| 恢复编排器 | `recovery_orchestrator.rs:76-91` | `attempt(kind)` 按 `WorkerFailureKind` → recipe；模拟 executor 默认返回 Recovered | ✅ |
| `WorkerFailureKind` | `worker_boot.rs:62-69` | TrustGate/ToolPermissionGate/PromptDelivery/Protocol/Provider/StartupNoEvidence | ✅ |
| NOTEBOOK 注入点 | `conversation.rs:2128-2131` | `dynamic_sections.push(notebook_prompt)`（每个 loop 迭代重建） | ✅ |
| `append_to_section` | `notebook.rs:320-336` | 追加行，无去重无容量 | ✅ |
| 容量裁剪先例 | `notebook.rs:337-360` | `append_evidence`（4K 从头部裁剪对齐行首） | ✅ |
| runtime 跨 turn 复用 | `claw-shell/src/agent.rs:314` | `Arc<ConversationRuntime>` 每 turn 复用（跨 turn 持久化可行） | ✅ |

---

## 二、根因与方案总览

**根因链**：助手丢失"已尝试过什么、结果如何"的状态（Bug-1 检索失效 + NOTEBOOK 记账依赖 LLM 主动调用）→ 从零重建上下文 → 开始重复诊断；循环检测无执行权且每 turn 失效（Bug-2）→ 循环无人打断；唯一硬约束是迭代上限，到顶时无诊断无恢复（Bug-3）→ 表现为会话卡死。

| 任务 | 层 | 解决 | 主要文件 |
|------|----|------|---------|
| Task 1 | P0-检索 | `content_raw` 列 + v3 迁移，检索结果返回原文 | `history_search.rs` |
| Task 2 | P0-预防 | 失败工具调用自动记入 `<attempted>` 段 | `notebook.rs`, `conversation.rs` |
| Task 3 | P1-检测 | LoopDetector 跨 turn 衰减 + 输出规范化 | `loop_detection.rs`, `conversation.rs` |
| Task 4 | P1-执行 | Loop Abort 真正终止 turn（成功+失败路径都接入） | `conversation.rs` |
| Task 5 | P2-兜底 | 迭代上限 / abort 错误携带诊断上下文 | `conversation.rs`, `loop_detection.rs` |
| Task 6 | 验证 | 全量测试 + 更新 bug-fixes-session-loop.md | 文档 |

**已否决的方案（经代码核查后排除）**：
- trigram tokenizer 双轨（≥3 字走 trigram、<3 字走单字 AND）：rusqlite 0.31 支持，但 2 字查询（`飞书`）不命中，复杂度高，风险大 → 保留现单字切分方案，只修显示污染。
- Loop Abort 走 `WorkerFailureKind::Protocol` 恢复路径：`loop_detection.rs:9` 注释声称如此但未实现；经核查 `attempt(Protocol)` 默认模拟 executor 返回 Recovered → 会多跑一轮 doomed 迭代再失败，且日志误记 `mcp_handshake_failure` → 不采用，改为直接诊断终止。
- 新增 `WorkerFailureKind::LoopDetected` + 新 recipe：恢复编排器面向 worker-boot（trust prompt / MCP handshake / 编译修复），与主 agent 循环场景不匹配，属于过度设计 → 不采用。

---

## Task 1: history_search `content_raw` 列（P0-检索）

**Files:**
- Modify: `rust/crates/runtime/src/history_search.rs`
- Test: 同文件 tests 模块

- [ ] **Step 1.1: 写失败测试**

在 `history_search.rs` tests 模块（`search_chinese_query_finds_matches` 附近）追加：

```rust
#[test]
fn index_message_keeps_raw_content_for_cjk() {
    // 索引 CJK 消息后,检索命中的 content 必须是原始文本(不含切分空格)
    let (_file, index) = open_temp_index();
    let raw = "如何配置飞书机器人 Webhook";
    index
        .index_message(raw, "sess-a", "user", 0, 1_000)
        .expect("index msg");
    let hits = index.search("飞书", 10).expect("search 飞书");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].content, raw,
        "hit.content 必须是原始文本,而不是切分后的 '如 何 配 置 飞 书 ...'"
    );
}
```

- [ ] **Step 1.2: 运行确认失败**

```bash
cd rust && cargo test -p runtime history_search::tests::index_message_keeps_raw_content_for_cjk
```

Expected: FAIL — `hit.content` 含空格（`如 何 配 置 飞 书 机 器 人 Webhook`）。

- [ ] **Step 1.3: 实现 content_raw 列**

**1.3.1 重构 `open()`（L52-87）**——版本检测改为 v1/v2 双路径：

```rust
    pub fn open(db_path: &Path) -> Result<Self, HistoryIndexError> {
        // Create parent directory (e.g. `.claw/`) if missing — prevents
        // silent failure where history_index stays None and session_search
        // becomes permanently unavailable for the session.
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut conn = Connection::open(db_path)?;
        // 版本检测与迁移:
        // - v1:有 history 表但无 history_meta(未切分 CJK)→ 重建为 v3(带 content_raw)
        // - v2:有 history_meta 且 schema_version < 3(content 已切分但无 content_raw)→ 重建为 v3
        let has_history_table = table_exists(&conn, "history");
        let has_meta = table_exists(&conn, "history_meta");
        if has_history_table && !has_meta {
            migrate_from_v1(&mut conn)?;
        } else if has_meta && current_schema_version(&conn)? < 3 {
            migrate_to_v3(&mut conn)?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS history_meta (\
                 key TEXT PRIMARY KEY,\
                 value TEXT NOT NULL\
             );\
             INSERT OR REPLACE INTO history_meta (key, value)\
                 VALUES ('schema_version', '3');\
             CREATE VIRTUAL TABLE IF NOT EXISTS history USING fts5(\
                 content,\
                 content_raw UNINDEXED,\
                 session_id UNINDEXED,\
                 role UNINDEXED,\
                 message_index UNINDEXED,\
                 timestamp_ms UNINDEXED\
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
```

**1.3.2 `index_message`（L92-113）**——content 列存切分文本，content_raw 列存原文：

```rust
        conn.execute(
            "INSERT INTO history (content, content_raw, session_id, role, message_index, timestamp_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                tokenize_content_for_index(content),
                content, // content_raw: 原始文本,供检索结果显示
                session_id,
                role,
                message_index as i64,
                timestamp_ms as i64,
            ],
        )?;
```

**1.3.3 `search`（L140-145）**——SELECT 改为优先 content_raw（防御 NULL 时回退 content）：

```rust
        let mut stmt = conn.prepare(
            "SELECT COALESCE(content_raw, content), session_id, role, message_index, timestamp_ms, rank \
             FROM history \
             WHERE history MATCH ?1 \
             ORDER BY rank \
             LIMIT ?2",
        )?;
```

> 注：若 FTS5 虚拟表对 `COALESCE` 表达式报错（Step 1.5 会立即暴露），退化为直接 `SELECT content_raw, ...` —— v3 迁移与全部新插入都显式写入 content_raw，无 NULL，COALESCE 仅防御。

**1.3.4 新增辅助函数**（放在 `tokenize_query_for_match` 之后）：

```rust
/// 逆变换 [`tokenize_content_for_index`]:去掉"汉字后插入的空格",还原原始文本。
///
/// v2 索引的 content 列存的是切分文本(如 `继 续 帮 我 配 置 飞 书 `),迁移到 v3
/// 时用它还原显示文本。规则:
/// - 汉字后紧跟一个空格:该空格是插入的,丢弃(汉字后紧跟汉字/标点/结尾时)。
/// - 汉字后紧跟两个空格:第一个是插入的,第二个是原文的空格,保留一个。
/// - 其余字符原样保留。
fn detokenize_content(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if is_han(c) && i + 1 < chars.len() && chars[i + 1] == ' ' {
            if i + 2 < chars.len() && chars[i + 2] == ' ' {
                // 汉字 + 插入空格 + 原空格:保留一个空格
                out.push(c);
                out.push(' ');
                i += 3;
            } else {
                // 汉字 + 插入空格:丢弃空格
                out.push(c);
                i += 2;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 读取 history_meta 中的 schema_version(表/键缺失时返回 0,触发迁移)。
fn current_schema_version(conn: &Connection) -> Result<i64, HistoryIndexError> {
    Ok(conn
        .query_row(
            "SELECT value FROM history_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0))
}
```

**1.3.5 `migrate_from_v1`（L347-390）**——INSERT 增加 content_raw（v1 存的是原始文本，直接回填）：

```rust
        for (content, session_id, role, message_index, timestamp_ms) in &legacy {
            tx.execute(
                "INSERT INTO history (content, content_raw, session_id, role, message_index, timestamp_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    tokenize_content_for_index(content),
                    content, // v1 存的是原始文本,直接回填
                    session_id,
                    role,
                    message_index,
                    timestamp_ms,
                ],
            )?;
        }
```

**1.3.6 新增 `migrate_to_v3`**（v2 → v3，放在 `migrate_from_v1` 之后）：

```rust
/// 迁移 v2 索引(schema_version=2,content 已切分、无 content_raw 列)到 v3:
/// 重建表并回填 content_raw。v2 的 content 列存的是切分文本,无法直接还原原文,
/// 用 [`detokenize_content`] 逆变换(去掉汉字后插入的空格)近似还原显示文本。
fn migrate_to_v3(conn: &mut Connection) -> Result<(), HistoryIndexError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "SELECT content, session_id, role, message_index, timestamp_ms FROM history",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut legacy: Vec<(String, String, String, i64, i64)> = Vec::new();
        for row in rows {
            legacy.push(row?);
        }
        drop(stmt);
        tx.execute_batch("DROP TABLE IF EXISTS history;")?;
        tx.execute_batch(
            "CREATE VIRTUAL TABLE history USING fts5(\
                 content,\
                 content_raw UNINDEXED,\
                 session_id UNINDEXED,\
                 role UNINDEXED,\
                 message_index UNINDEXED,\
                 timestamp_ms UNINDEXED\
             );",
        )?;
        for (content, session_id, role, message_index, timestamp_ms) in &legacy {
            tx.execute(
                "INSERT INTO history (content, content_raw, session_id, role, message_index, timestamp_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    content, // 已是切分文本,原样保留(索引 token 不变)
                    detokenize_content(content),
                    session_id,
                    role,
                    message_index,
                    timestamp_ms,
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}
```

- [ ] **Step 1.4: 补迁移与逆变换测试**

在 tests 模块追加：

```rust
#[test]
fn migration_v2_backfills_content_raw() {
    // 构造 v2 索引:history_meta 存在且 schema_version=2,content 已切分,无 content_raw 列
    let file = NamedTempFile::new().expect("create temp db file");
    {
        let conn = rusqlite::Connection::open(file.path()).expect("open conn");
        conn.execute_batch(
            "CREATE TABLE history_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO history_meta VALUES ('schema_version', '2');
             CREATE VIRTUAL TABLE history USING fts5(
                 content,
                 session_id UNINDEXED,
                 role UNINDEXED,
                 message_index UNINDEXED,
                 timestamp_ms UNINDEXED
             );
             INSERT INTO history VALUES ('继 续 帮 我 配 置 飞 书 ', 'sess-v2', 'user', 0, 1000);",
        )
        .expect("create v2 table");
    }

    let index = HistoryIndex::open(file.path()).expect("open migrates v2 to v3");
    let hits = index.search("飞书", 10).expect("search 飞书 after v3 migration");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].content, "继续帮我配置飞书",
        "content_raw 必须由切分文本逆变换还原"
    );
    // 二次 open 不重复迁移(幂等)
    let index2 = HistoryIndex::open(file.path()).expect("open again");
    assert_eq!(index2.count().expect("count after second open"), 1);
}

#[test]
fn detokenize_content_reconstructs_raw_text() {
    // 纯汉字串
    assert_eq!(detokenize_content("继 续 帮 我 配 置 飞 书 "), "继续帮我配置飞书");
    // 汉字 + 原文空格
    assert_eq!(detokenize_content("飞 书  配 置 "), "飞书 配置");
    // 汉字 + 英文(无空格)
    assert_eq!(detokenize_content("飞 书 Feishu"), "飞书Feishu");
    // 汉字 + 原文空格 + 英文
    assert_eq!(detokenize_content("飞 书  Feishu"), "飞书 Feishu");
    // 无汉字:原样
    assert_eq!(detokenize_content("the quick brown fox"), "the quick brown fox");
    // 汉字 + 标点
    assert_eq!(detokenize_content("配 置 完 成 。"), "配置完成。");
}
```

同时更新既有测试 `migration_reindexes_legacy_cjk_content`：在 `search 飞书 after migration` 断言后追加一行

```rust
        assert_eq!(hits[0].content, "继续帮我配置飞书机器人");
```

（若既有测试文件里该断言行号不同，按实际位置追加。）

- [ ] **Step 1.5: 运行全部 history_search 测试确认通过**

```bash
cd rust && cargo test -p runtime history_search
```

Expected: 全部 PASS（含 Step 1.1 新测试、Step 1.4 迁移/逆变换测试、既有 CJK 测试）。

- [ ] **Step 1.6: 提交**

```bash
git add rust/crates/runtime/src/history_search.rs
git commit -m "fix(runtime): history FTS 增加 content_raw 列,检索结果返回原文而非切分文本"
```

---

## Task 2: 失败工具调用自动记入 NOTEBOOK `<attempted>`（P0-预防）

**Files:**
- Modify: `rust/crates/runtime/src/notebook.rs`
- Modify: `rust/crates/runtime/src/conversation.rs:2774-2787`（工具结果 merge 后插入）
- Test: 两文件 tests 模块

- [ ] **Step 2.1: 写失败测试**

在 `notebook.rs` tests 模块追加（若 tests 模块未 `use tempfile`，先补 `use tempfile::tempdir;`）：

```rust
#[test]
fn append_attempt_records_and_dedups() {
    let dir = tempdir().expect("tempdir");
    append_attempt(dir.path(), "Bash", "netstat -an", "no output").expect("append");
    append_attempt(dir.path(), "Bash", "netstat -an", "no output").expect("append again");
    let nb = Notebook::load(dir.path()).expect("load");
    let sec = nb.get_section("attempted").expect("attempted section exists");
    assert_eq!(sec.lines().count(), 1, "完全相同的尝试只记录一次");
    assert!(sec.contains("netstat -an"));
}

#[test]
fn append_attempt_caps_section_size() {
    let dir = tempdir().expect("tempdir");
    for i in 0..100 {
        append_attempt(dir.path(), "Bash", &format!("cmd {i}"), "failed").expect("append");
    }
    let nb = Notebook::load(dir.path()).expect("load");
    let sec = nb.get_section("attempted").expect("attempted");
    assert!(
        sec.chars().count() <= ATTEMPTED_MAX_CHARS,
        "attempted 段必须被裁剪到容量内"
    );
    assert!(sec.contains("cmd 99"), "保留最新的尝试");
    assert!(!sec.contains("cmd 0"), "最旧的尝试被裁剪");
}
```

Expected: FAIL — `append_attempt` / `ATTEMPTED_MAX_CHARS` 未定义（编译错误）。

- [ ] **Step 2.2: 运行确认失败**

```bash
cd rust && cargo test -p runtime notebook::tests::append_attempt_records_and_dedups
```

Expected: 编译失败，报 `cannot find function append_attempt`。

- [ ] **Step 2.3: 实现 `append_attempt`**

在 `notebook.rs` 的 `append_evidence`（L337-360）之后追加：

```rust
/// `<attempted>` 段自动记录的最大字符数。超出时从头部裁剪,保留最新的失败尝试。
pub const ATTEMPTED_MAX_CHARS: usize = 2048;

/// 运行时自动追加一条失败尝试到 `<attempted>` 段(不依赖 LLM 主动调用)。
///
/// 循环中的 LLM 不会停下来调用 `notebook_update` 记账,本函数由运行时在
/// 工具调用失败路径自动调用,使下一轮/下一 turn 的 prompt 注入能看到
/// "已尝试且失败"的路径,从源头消除重复诊断。
///
/// 特性:
/// - 去重:完全相同的尝试行不重复追加(同一失败循环只记 1 条,不膨胀 prompt)
/// - 容量:超出 [`ATTEMPTED_MAX_CHARS`] 时从头部裁剪(对齐行首,不截断半行)
/// - 失败静默:NOTEBOOK 读写失败返回 Err,由调用方吞掉(不阻塞主流程)
pub fn append_attempt(
    workspace_root: &Path,
    tool_name: &str,
    tool_input: &str,
    output: &str,
) -> Result<(), NotebookError> {
    let mut notebook = Notebook::load(workspace_root)?;
    let line = format!(
        "- [tool] {tool_name} | input={} | failed: {}",
        truncate_for_attempt(tool_input, 80),
        truncate_for_attempt(output, 120),
    );
    let already = notebook
        .get_section("attempted")
        .map_or(false, |s| s.lines().any(|l| l.trim() == line));
    if already {
        return Ok(());
    }
    notebook.append_to_section("attempted", &line);
    if let Some(sec) = notebook.get_section("attempted") {
        if sec.chars().count() > ATTEMPTED_MAX_CHARS {
            let overflow = sec.chars().count() - ATTEMPTED_MAX_CHARS;
            let skipped: String = sec.chars().skip(overflow).collect();
            let trimmed = skipped
                .find('\n')
                .map(|nl| skipped[nl + 1..].to_string())
                .unwrap_or(skipped);
            notebook.set_section("attempted", &trimmed);
        }
    }
    notebook.save(workspace_root)
}

/// 按字符数截断文本,超出时加省略号。
fn truncate_for_attempt(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}
```

> 若 `notebook.rs` 顶部未 `use std::path::Path;`（`load` 已用 `&Path`，应有），在 use 区补上。

- [ ] **Step 2.4: 在工具失败路径接线**

在 `conversation.rs` 工具循环中，`output = merge_hook_feedback(post_hook_result.messages(), output, ...)`（L2774-2787）之后、`ConversationMessage::tool_result(...)` 之前插入：

```rust
                        // P0:失败的工具调用自动记录到 NOTEBOOK <attempted> 段。
                        // 循环中的 LLM 不会主动调用 notebook_update,此处由运行时记账,
                        // 使下一轮/下一 turn 看到"已尝试且失败"的路径,从源头消除重复诊断。
                        // 静默吞错:记录失败不阻断工具结果返回(与历史索引 hook 一致)。
                        if is_error {
                            if let Some(workspace_root) = &self.workspace_root {
                                let _ = crate::notebook::append_attempt(
                                    workspace_root,
                                    &tool_name,
                                    &effective_input,
                                    &output,
                                );
                            }
                        }
```

- [ ] **Step 2.5: 运行测试确认通过**

```bash
cd rust && cargo test -p runtime notebook::tests::append_attempt
```

Expected: 全部 PASS。

- [ ] **Step 2.6: 提交**

```bash
git add rust/crates/runtime/src/notebook.rs rust/crates/runtime/src/conversation.rs
git commit -m "fix(runtime): 失败工具调用自动记入 NOTEBOOK attempted 段,防重复诊断循环"
```

---

## Task 3: LoopDetector 跨 turn 衰减 + 输出规范化（P1-检测）

**Files:**
- Modify: `rust/crates/runtime/src/loop_detection.rs`
- Modify: `rust/crates/runtime/src/conversation.rs:1987`（每 turn reset 替换）
- Test: `loop_detection.rs` tests 模块

- [ ] **Step 3.1: 写失败测试（跨 turn 计数 + 衰减 + 输出规范化）**

在 `loop_detection.rs` tests 模块追加：

```rust
#[test]
fn tool_call_counts_survive_across_turns_within_window() {
    // 模拟跨 turn:turn1 记录 3 次,turn2(仍在窗口内)继续累积到中止阈值
    let mut detector = LoopDetector::new();
    let now = 1_000_000u64;
    for _ in 0..TOOL_WARN_THRESHOLD {
        let _ = detector.record_tool_call_at("Bash", "netstat", "LISTENING", now);
    }
    // 下一 turn,5 分钟后(仍在 15 分钟窗口内)继续相同调用
    let mut aborted = false;
    // 仅补到第 5 次(4、5 次应为 Continue/InjectContext),第 6 次留给循环外的最终断言
    for _ in TOOL_WARN_THRESHOLD..TOOL_ABORT_THRESHOLD - 1 {
        let action = detector.record_tool_call_at("Bash", "netstat", "LISTENING", now + 5 * 60 * 1000);
        if matches!(action, LoopAction::Abort(_)) {
            aborted = true;
        }
    }
    assert!(!aborted, "第 4-5 次应仍为 Continue/InjectContext");
    let action = detector.record_tool_call_at("Bash", "netstat", "LISTENING", now + 5 * 60 * 1000);
    assert!(
        matches!(action, LoopAction::Abort(_)),
        "跨 turn 循环(窗口内)应被检测: {action:?}"
    );
}

#[test]
fn prune_decayed_removes_stale_tool_calls() {
    let mut detector = LoopDetector::new();
    let now = 1_000_000u64;
    for _ in 0..TOOL_WARN_THRESHOLD {
        let _ = detector.record_tool_call_at("Bash", "netstat", "", now);
    }
    // 时间流逝超过窗口 → 计数清空
    detector.prune_decayed(now + 20 * 60 * 1000, 15 * 60 * 1000);
    // 重新计数:前 2 次仍 Continue(而非立即警告/中止)
    for _ in 0..TOOL_WARN_THRESHOLD - 1 {
        let action = detector.record_tool_call_at("Bash", "netstat", "", now + 21 * 60 * 1000);
        assert!(matches!(action, LoopAction::Continue), "prune 后计数应重置: {action:?}");
    }
}

#[test]
fn normalize_output_strips_timestamps_and_whitespace() {
    assert_eq!(normalize_output("2026-08-09T01:26:43.123Z listening"), "TS listening");
    assert_eq!(normalize_output("   abc   def  \r\n "), "abc def");
    assert_eq!(normalize_output("error 404: page not found"), "error 404: page not found");
    assert_eq!(normalize_output("62112"), "62112"); // 纯数字端口号不受影响
}

#[test]
fn identical_output_with_different_timestamps_is_detected() {
    // tail -f 日志:每条输出带不同时间戳 → 规范化后视为相同输出,触发验证循环检测
    let mut detector = LoopDetector::new();
    for i in 0..SAME_OUTPUT_WARN_THRESHOLD - 1 {
        let out = format!("2026-08-09T01:26:{i:02}.123Z still waiting");
        let _ = detector.record_tool_call("Bash", &format!("sleep {} && tail log", i), &out);
    }
    let action = detector.record_tool_call(
        "Bash",
        "sleep 9 && tail log",
        "2026-08-09T02:00:00.000Z still waiting",
    );
    assert!(matches!(action, LoopAction::InjectContext(_)), "时间戳不同的相同输出应触发警告: {action:?}");
}
```

Expected: FAIL — `record_tool_call_at` / `prune_decayed` / `normalize_output` 未定义。

- [ ] **Step 3.2: 运行确认失败**

```bash
cd rust && cargo test -p runtime loop_detection::tests::tool_call_counts_survive_across_turns_within_window
```

Expected: 编译失败，报找不到函数。

- [ ] **Step 3.3: 实现跨 turn 衰减 + 输出规范化**

**3.3.1 结构字段类型**（L63-86）：`(count, last_seen_ms)` 二元组替代纯 u32：

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopDetector {
    /// 每个文件路径的编辑计数。
    edit_counts: HashMap<String, u32>,
    /// 累计总编辑次数(跨所有文件)。
    total_edits: u64,
    /// 是否已经对该文件发出过警告(避免重复注入)。
    warned: HashMap<String, bool>,
    /// (tool_name, 规范化 input) → (调用次数, 最后调用时间戳 ms)。
    /// 时间戳用于跨 turn 衰减:窗口内跨 turn 累积,超时自动清零。
    tool_call_counts: HashMap<(String, String), (u32, u64)>,
    /// (tool_name, 规范化 output) → (出现次数, 最后出现时间戳 ms)。
    tool_output_counts: HashMap<(String, String), (u32, u64)>,
    /// 已发出过警告的调用 key → 警告时间戳 ms。衰减时一并清除,
    /// 允许窗口期过后对新一轮循环重新警告。
    tool_warned: HashMap<String, u64>,
}
```

**3.3.2 `record_tool_call` 拆分为带时间戳的私有版本**（L150-214 替换）：

```rust
    /// 记录一次工具调用,检测诊断/验证循环。对所有工具生效(不只文件编辑)。
    ///
    /// 两个信号:
    /// - **完全相同调用**:`(tool_name, 规范化 input)` 相同。阈值
    ///   [`TOOL_WARN_THRESHOLD`] / [`TOOL_ABORT_THRESHOLD`](3/6)。
    /// - **输出无变化**:`(tool_name, 规范化 output)` 相同(输入可能不同,
    ///   如 `sleep 3 && tail` 与 `sleep 5 && tail` 输出相同)。输出先经
    ///   [`normalize_output`] 剥离时间戳/折叠空白,使带时间戳的日志输出
    ///   也能命中。阈值 [`SAME_OUTPUT_WARN_THRESHOLD`] /
    ///   [`SAME_OUTPUT_ABORT_THRESHOLD`](5/10)。
    ///
    /// 计数按时间戳保留,跨 turn 有效(由 [`LoopDetector::prune_decayed`]
    /// 衰减);优先返回完全相同调用的动作。
    #[must_use]
    pub fn record_tool_call(&mut self, tool_name: &str, tool_input: &str, output: &str) -> LoopAction {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.record_tool_call_at(tool_name, tool_input, output, now_ms)
    }

    /// [`record_tool_call`] 的时间戳注入版本(可测试)。
    fn record_tool_call_at(
        &mut self,
        tool_name: &str,
        tool_input: &str,
        output: &str,
        now_ms: u64,
    ) -> LoopAction {
        let normalized = normalize_tool_input(tool_input);
        let call_key = (tool_name.to_owned(), normalized);
        let entry = self.tool_call_counts.entry(call_key.clone()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 = now_ms;

        if entry.0 >= TOOL_ABORT_THRESHOLD {
            return LoopAction::Abort(format!(
                "doom loop detected: tool '{tool_name}' invoked {} times with identical input",
                entry.0
            ));
        }
        let mut action = LoopAction::Continue;
        if entry.0 == TOOL_WARN_THRESHOLD {
            let warn_key = format!("call:{tool_name}:{}", call_key.1);
            if !self.tool_warned.contains_key(&warn_key) {
                self.tool_warned.insert(warn_key, now_ms);
                action = LoopAction::InjectContext(format!(
                    "consider reconsidering your approach — tool '{tool_name}' has been invoked \
                     {} times with identical input; the result has not changed",
                    entry.0
                ));
            }
        }

        // 输出无变化信号(输入不同但结果相同);输出先规范化(剥离时间戳等易变部分)
        let normalized_output = normalize_output(output);
        let out_key = (tool_name.to_owned(), normalized_output);
        let out_entry = self.tool_output_counts.entry(out_key.clone()).or_insert((0, 0));
        out_entry.0 += 1;
        out_entry.1 = now_ms;
        if out_entry.0 >= SAME_OUTPUT_ABORT_THRESHOLD {
            return LoopAction::Abort(format!(
                "doom loop detected: tool '{tool_name}' returned identical output {} times",
                out_entry.0
            ));
        }
        if out_entry.0 == SAME_OUTPUT_WARN_THRESHOLD && matches!(action, LoopAction::Continue) {
            let warn_key = format!("out:{tool_name}:{}", out_key.1);
            if !self.tool_warned.contains_key(&warn_key) {
                self.tool_warned.insert(warn_key, now_ms);
                action = LoopAction::InjectContext(format!(
                    "consider reconsidering your approach — tool '{tool_name}' returned identical \
                     output {} times; the result has not changed, consider changing strategy \
                     or asking the user",
                    out_entry.0
                ));
            }
        }
        action
    }
```

**3.3.3 拆 `reset()` 为 `reset_edits()` + `prune_decayed()`**（替换 L216-223 附近）：

```rust
    /// 重置文件编辑跟踪(每个 turn 开始调用;工具调用计数保留,支持跨 turn 检测)。
    pub fn reset_edits(&mut self) {
        self.edit_counts.clear();
        self.warned.clear();
        self.total_edits = 0;
    }

    /// 按时间窗口衰减工具调用计数:超过 `max_age_ms` 未出现的调用从统计中移除。
    /// 工具调用跨 turn 保留(窗口内);文件编辑计数不受影响(每 turn 由
    /// [`LoopDetector::reset_edits`] 清空)。
    pub fn prune_decayed(&mut self, now_ms: u64, max_age_ms: u64) {
        self.tool_call_counts
            .retain(|_, (_, last_seen)| now_ms.saturating_sub(*last_seen) <= max_age_ms);
        self.tool_output_counts
            .retain(|_, (_, last_seen)| now_ms.saturating_sub(*last_seen) <= max_age_ms);
        self.tool_warned
            .retain(|_, warned_at| now_ms.saturating_sub(*warned_at) <= max_age_ms);
    }
```

**3.3.4 新增 `normalize_output`**（放在 `normalize_tool_input` 之后）：

```rust
/// 规范化工具输出,使"语义相同、文本不同"的调用互相匹配(验证循环检测):
/// - 剥离时间戳类 token(ISO-8601 / 时钟时间,如 `2026-08-09T01:26:43.123Z`)→ `TS`
/// - 折叠连续空白(含 `\r\n` → 单个空格)
///
/// 启发式:`is_timestamp_like` 判定 token 是否"长度 ≥ 8、全由数字与
/// `:-TZ.+` 组成且含至少一个分隔符"。纯数字(端口号等)不受影响。
fn normalize_output(output: &str) -> String {
    let mut out = String::with_capacity(output.len());
    let mut token = String::new();
    let mut flush = |out: &mut String, token: &mut String| {
        if !token.is_empty() {
            if is_timestamp_like(token) {
                out.push_str("TS");
            } else {
                out.push_str(token);
            }
            token.clear();
        }
    };
    for ch in output.chars() {
        if ch.is_whitespace() {
            flush(&mut out, &mut token);
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            token.push(ch);
        }
    }
    flush(&mut out, &mut token);
    out.trim().to_string()
}

/// 启发式:token 是否像时间戳(长度 ≥ 8,仅由数字与分隔符组成,含至少 1 个分隔符)。
fn is_timestamp_like(token: &str) -> bool {
    if token.chars().count() < 8 {
        return false;
    }
    let mut digits = 0usize;
    let mut seps = 0usize;
    for c in token.chars() {
        match c {
            '0'..='9' => digits += 1,
            ':' | '-' | 'T' | 'Z' | '.' | '+' => seps += 1,
            _ => return false,
        }
    }
    digits >= 8 && seps >= 1
}
```

- [ ] **Step 3.4: conversation.rs 每 turn reset 替换**

在 `conversation.rs:1987` 处，把

```rust
        // P2-7 修复:在每个 turn 开始时重置 loop_detector,避免跨 turn 累积。
        // 否则同一文件被多次编辑会触发 InjectContext/Abort,即使这些编辑分布在
        // 不同 turn 中(误判 doom loop)。
        self.loop_detector.reset();
```

替换为：

```rust
        // P2-7 修复(升级 v2):跨 turn 循环检测。
        // 原实现每 turn 全量 reset(),跨 turn 的"换参数再诊断"循环不可见
        // (turn 1 诊断失败 → turn 2 换参数再诊断 → ...)。现改为:
        // - 文件编辑计数每 turn 清空(reset_edits):避免多 turn 合法编辑被误判;
        // - 工具调用计数按时间窗口衰减(prune_decayed):窗口内跨 turn 累积,
        //   超时自动清零,兼顾"跨 turn 循环检测"与"合法重复检查"。
        self.loop_detector.reset_edits();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.loop_detector.prune_decayed(now_ms, LOOP_DECAY_WINDOW_MS);
```

并在 `DEFAULT_MAX_ITERATIONS`（L769）附近追加常量：

```rust
/// 工具调用循环检测的跨 turn 保留窗口(15 分钟)。
/// 窗口内相同工具调用跨 turn 累积计数;超过窗口未出现则衰减清零。
pub const LOOP_DECAY_WINDOW_MS: u64 = 15 * 60 * 1000;
```

- [ ] **Step 3.5: 运行测试确认通过**

```bash
cd rust && cargo test -p runtime loop_detection
```

Expected: 全部 PASS（含既有 9 个循环检测测试与 Step 3.1 新增 4 个）。

- [ ] **Step 3.6: 提交**

```bash
git add rust/crates/runtime/src/loop_detection.rs rust/crates/runtime/src/conversation.rs
git commit -m "fix(runtime): LoopDetector 工具调用计数跨 turn 衰减 + 输出规范化,捕获跨 turn 诊断循环"
```

---

## Task 4: Loop Abort 真正终止 turn（P1-执行）

**Files:**
- Modify: `rust/crates/runtime/src/conversation.rs`
- Test: 同文件 tests 模块

- [ ] **Step 4.1: 写失败测试**

在 `conversation.rs` tests 模块（`run_turn_errors_when_max_iterations_is_exceeded` 之后）追加：

```rust
#[test]
fn run_turn_aborts_early_on_tool_loop() {
    struct LoopingApi;

    impl ApiClient for LoopingApi {
        fn stream(
            &mut self,
            _request: ApiRequest,
        ) -> Result<Vec<AssistantEvent>, RuntimeError> {
            Ok(vec![
                AssistantEvent::ToolUse {
                    id: "tool-1".to_string(),
                    name: "echo".to_string(),
                    input: "payload".to_string(),
                },
                AssistantEvent::MessageStop,
            ])
        }
    }

    // max_iterations=64 远高于中止阈值(6),证明是 loop detector 而非迭代上限兜底
    let mut runtime = ConversationRuntime::new(
        Session::new(),
        LoopingApi,
        StaticToolExecutor::new().register("echo", |input| Ok(input.to_string())),
        PermissionPolicy::new(PermissionMode::DangerFullAccess),
        vec!["system".to_string()],
    )
    .with_max_iterations(64);

    // when
    let error = runtime
        .run_turn("loop", None)
        .expect_err("identical tool calls should abort the turn");

    // then
    assert!(
        error.to_string().contains("doom loop detected"),
        "unexpected error: {error}"
    );
}
```

Expected: FAIL — 原实现把 Abort 仅标为工具错误，turn 继续跑到 64 次迭代，错误消息是 `conversation loop exceeded...`，断言不匹配。

- [ ] **Step 4.2: 运行确认失败**

```bash
cd rust && cargo test -p runtime run_turn_aborts_early_on_tool_loop
```

Expected: FAIL（断言 `doom loop detected` 不满足）。

- [ ] **Step 4.3: 实现——字段 + 共享检测方法 + 两条 hook 路径 + 终止点**

**4.3.1 新增字段**：在 `loop_detector: LoopDetector,`（L827）之后加：

```rust
    /// LoopDetector Abort 触发时记录的原因;工具循环看到 Some 立即终止 turn。
    /// 与 hook 的 cancelled 标志区分:普通 hook 取消只把工具结果标错,
    /// loop abort 则真正中断 turn。
    loop_abort_reason: Option<String>,
```

在 `new()`（L994 `loop_detector: LoopDetector::new(),` 附近）初始化：

```rust
            loop_abort_reason: None,
```

**4.3.2 提取共享检测方法**（放在 `run_post_tool_use_hook` 之前）：

```rust
    /// 运行 LoopDetector(文件编辑 + 工具调用双通道),合并动作并返回。
    /// Abort 时同时写入 `loop_abort_reason`,供工具循环识别并终止 turn。
    fn apply_loop_detection(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> LoopAction {
        let mut action = LoopAction::Continue;
        if let Some(file_path) = extract_file_path_from_tool_input(tool_name, input) {
            match self.loop_detector.record_edit(&file_path) {
                LoopAction::Abort(reason) => return LoopAction::Abort(reason),
                LoopAction::InjectContext(msg) => action = LoopAction::InjectContext(msg),
                LoopAction::Continue => {}
            }
        }
        match self.loop_detector.record_tool_call(tool_name, input, output) {
            LoopAction::Abort(reason) => return LoopAction::Abort(reason),
            LoopAction::InjectContext(msg) => {
                action = match action {
                    LoopAction::InjectContext(existing) => {
                        LoopAction::InjectContext(format!("{existing}\n{msg}"))
                    }
                    _ => LoopAction::InjectContext(msg),
                };
            }
            LoopAction::Continue => {}
        }
        action
    }
```

**4.3.3 整体替换 `run_post_tool_use_hook`**（L1848-1950）——整个函数体替换为：

```rust
        // BUG-2 修复:在 PostToolUse hook 中接入 LoopDetector(两个检测维度见
        // apply_loop_detection)。处理:
        // - Continue:正常流程,继续走原 hook_runner。
        // - InjectContext:把警告消息附加到 hook 结果的 messages 中,
        //   让主 agent 在下一轮看到"重新考虑方法"的提示。
        // - Abort:记录 loop_abort_reason 并返回 cancelled=true 的 HookRunResult,
        //   工具循环检测到 loop_abort_reason 后**真正终止 turn**(而非仅标错)。
        match self.apply_loop_detection(tool_name, input, output) {
            LoopAction::Abort(reason) => {
                self.loop_abort_reason = Some(reason.clone());
                return HookRunResult::cancelled_with_message(reason);
            }
            LoopAction::InjectContext(msg) => {
                let mut base_result = self.run_post_tool_use_hook_base(
                    tool_name, input, output, is_error,
                );
                base_result.append_message(msg);
                return base_result;
            }
            LoopAction::Continue => {}
        }
        self.run_post_tool_use_hook_base(tool_name, input, output, is_error)
```

**4.3.4 提取 hook 调用基座**（原 hook 尾部两段重复的 hook_runner 调用，抽成私有方法）：

```rust
    /// 执行真正的 PostToolUse hook(不含 loop 检测前置)。
    fn run_post_tool_use_hook_base(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
        is_error: bool,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }
```

**4.3.5 改写 `run_post_tool_use_failure_hook`**（L1952-1973）——失败路径同样接入循环检测：

```rust
    fn run_post_tool_use_failure_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> HookRunResult {
        // BUG-2 修复(升级):失败的工具调用同样进入循环检测。
        // 原实现失败路径完全绕过 LoopDetector,"命令报错 → 换参数再报错"的
        // 循环(exit != 0)无法被捕获。现在与成功路径对称处理。
        match self.apply_loop_detection(tool_name, input, output) {
            LoopAction::Abort(reason) => {
                self.loop_abort_reason = Some(reason.clone());
                return HookRunResult::cancelled_with_message(reason);
            }
            LoopAction::InjectContext(msg) => {
                let mut base_result = self.run_post_tool_use_failure_hook_base(
                    tool_name, input, output,
                );
                base_result.append_message(msg);
                return base_result;
            }
            LoopAction::Continue => {}
        }
        self.run_post_tool_use_failure_hook_base(tool_name, input, output)
    }

    /// 执行真正的 PostToolUseFailure hook(不含 loop 检测前置)。
    fn run_post_tool_use_failure_hook_base(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }
```

**4.3.6 工具循环终止点**：在 `output = merge_hook_feedback(post_hook_result.messages(), output, ...)`（L2774-2787）之后、`ConversationMessage::tool_result(...)` 之前插入：

```rust
                        // BUG-2 修复(升级):LoopDetector Abort 现在真正终止 turn。
                        // 原实现只把工具结果标记为 error,LLM 看到错误消息后仍会
                        // 继续循环,只有 64 次迭代上限兜底。现在 Abort 立即返回
                        // 带诊断的错误;已尝试记录在 NOTEBOOK <attempted> 段
                        // (Task 2 自动记账),供下一 turn 改变策略。
                        if let Some(reason) = self.loop_abort_reason.take() {
                            let error = RuntimeError::new(format!(
                                "doom loop detected, turn aborted: {reason}. \
                                 Failed attempts are recorded in the NOTEBOOK \
                                 <attempted> section; change strategy or ask the \
                                 user before retrying."
                            ));
                            self.record_turn_failed(iterations, &error);
                            return Err(error);
                        }
```

- [ ] **Step 4.4: 运行测试确认通过**

```bash
cd rust && cargo test -p runtime run_turn_aborts_early_on_tool_loop
cargo test -p runtime run_turn_errors_when_max_iterations_is_exceeded
```

Expected: 两个测试都 PASS。若其他既有测试因"提前 abort / 失败路径 InjectContext 消息"出现回归，按 Step 6.2 全量跑并逐个核对（多数是断言工具结果精确内容、且同一工具重复 ≥3 次的用例，属预期行为变化，更新断言即可）。

- [ ] **Step 4.5: 提交**

```bash
git add rust/crates/runtime/src/conversation.rs
git commit -m "fix(runtime): LoopDetector Abort 真正终止 turn(成功+失败路径均接入)"
```

---

## Task 5: 迭代上限 / abort 错误携带诊断上下文（P2-兜底）

**Files:**
- Modify: `rust/crates/runtime/src/conversation.rs:2084`
- Modify: `rust/crates/runtime/src/loop_detection.rs:7-9`（模块文档注释）
- Test: 无新增（消息断言由既有测试覆盖，见 Step 5.3）

- [ ] **Step 5.1: max_iterations 错误消息诊断化**

把 `conversation.rs:2084-2089`：

```rust
            if iterations > self.max_iterations {
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations",
                );
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }
```

替换为：

```rust
            if iterations > self.max_iterations {
                // BUG-3 修复(升级):超限错误携带诊断上下文。
                // 原实现裸错误,下一 turn 不知道上次为什么卡住 → 跨 turn 死循环
                // 仍可能复发。现在错误明确指向 NOTEBOOK <attempted> 段
                // (Task 2 已自动记录本 turn 所有失败尝试)。
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations (64). \
                     Turn aborted to prevent a runaway loop; failed attempts are \
                     recorded in the NOTEBOOK <attempted> section. Change strategy \
                     or ask the user before retrying.",
                );
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }
```

（保留原前缀 `conversation loop exceeded the maximum number of iterations`，既有测试 `run_turn_errors_when_max_iterations_is_exceeded` 的 `contains` 断言继续通过。）

- [ ] **Step 5.2: 修正 loop_detection.rs 模块文档的过时声明**

把 `loop_detection.rs:7-9`：

```rust
//! - [`LoopAction`]:中间件输出 — Continue / InjectContext / Abort。
//! - 与 [`RecoveryOrchestrator`](crate::recovery_orchestrator) 对接:Abort
//!   走 `WorkerFailureKind::Protocol` 恢复路径。
```

替换为：

```rust
//! - [`LoopAction`]:中间件输出 — Continue / InjectContext / Abort。
//! - Abort 行为(经分析修订):**不**走 RecoveryOrchestrator —— 恢复编排器面向
//!   worker-boot(trust prompt / MCP handshake / 编译修复),与主 agent 循环场景
//!   不匹配,且默认模拟 executor 会把恢复误报为成功、多跑一轮 doomed 迭代。
//!   实际由 conversation 工具循环检测到 `loop_abort_reason` 后**直接终止 turn**,
//!   诊断信息写入 NOTEBOOK `<attempted>` 段供下一轮改变策略。
```

- [ ] **Step 5.3: 运行既有迭代上限测试**

```bash
cd rust && cargo test -p runtime run_turn_errors_when_max_iterations_is_exceeded
```

Expected: PASS（消息前缀未变）。

- [ ] **Step 5.4: 提交**

```bash
git add rust/crates/runtime/src/conversation.rs rust/crates/runtime/src/loop_detection.rs
git commit -m "fix(runtime): 迭代上限/loop abort 错误携带 attempted 段诊断上下文"
```

---

## Task 6: 全量验证 + 文档更新

**Files:**
- Modify: `docs/bug-fixes-session-loop.md`
- 验证: 全量测试 + clippy

- [ ] **Step 6.1: 更新 bug-fixes-session-loop.md**

在 `## 四、涉及文件` 之后追加一节：

```markdown
---

## 五、第二轮加固（根本性修复，2026-08-09）

> 第一轮修复了 3 个机制缺陷的**症状**;第二轮解决**根因**:助手丢失"已尝试且失败"状态、
> 循环检测无执行权且跨 turn 失效、终止路径无诊断。实施计划见 `docs/plans/2026-08-09-loop-prevention.md`。

### 5.1 第一轮修复的 4 个残留缺陷

| 残留缺陷 | 位置 | 后果 |
|---------|------|------|
| 检索结果被 CJK 切分空格污染 | `history_search.rs` content 列 | 助手"回忆历史"读到的内容是 `继 续 帮 我 配 置 飞 书 ` |
| `<attempted>` 记账依赖 LLM 主动调用 | `notebook_update` | 循环中的 LLM 不会停下来记账 → 已尝试状态永远缺失 |
| 循环检测每 turn 全量 reset + Abort 仅标错 | `conversation.rs:1987` + `run_post_tool_use_hook` | 跨 turn 循环不可见;Abort 后 turn 继续 |
| 失败路径完全绕过循环检测 | `run_post_tool_use_failure_hook` | "命令报错 → 换参数再报错"循环无法捕获 |

### 5.2 本轮改动

- **P0-检索**：history FTS5 增加 `content_raw UNINDEXED` 列存原文，`search` 返回原文；v1/v2 → v3 自动迁移（v2 用逆变换还原切分文本）。
- **P0-预防**：失败工具调用由运行时自动记入 NOTEBOOK `<attempted>` 段（去重 + 容量裁剪），不依赖 LLM 主动调用。
- **P1-检测**：LoopDetector 工具调用计数按 15 分钟窗口跨 turn 衰减；输出规范化（剥离时间戳/折叠空白）；失败路径同样接入检测。
- **P1-执行**：Loop Abort 真正终止 turn（`loop_abort_reason` + 工具循环终止点），不再只把工具结果标错。
- **P2-兜底**：迭代上限与 abort 错误消息携带 `<attempted>` 段诊断指引。

### 5.3 验证结果

- 新增测试：Task 1 迁移/逆变换 3 个、Task 2 去重/容量 2 个、Task 3 跨 turn/衰减/规范化 4 个、Task 4 提前中止 1 个。
- `cargo test -p runtime` 全量通过;`cargo clippy -p runtime --all-targets -- -D warnings` 通过。
```

- [ ] **Step 6.2: 全量测试**

```bash
cd rust && cargo test -p runtime
```

Expected: 全部 PASS。若个别既有测试因"提前 abort / 失败路径 InjectContext 消息 / max_iterations 消息变化"失败，逐一定位：
- 消息 `contains` 断言类 → 检查是否依赖旧前缀（Task 5 已保留前缀，应不受影响）；
- 同工具重复 ≥3 次且断言工具结果精确内容 → 属预期行为变化（现在会附带"重新考虑方法"提示），更新断言或在测试中避免重复调用同一工具。

- [ ] **Step 6.3: Clippy**

```bash
cd rust && cargo clippy -p runtime --all-targets -- -D warnings
```

Expected: 无警告（注意：工作区其他 crate 如 im-bridge 有未提交改动，故本步限定 `-p runtime` 避免无关噪音；仓库规范的全量 `cargo clippy --workspace` 可在 im-bridge 改动落定后另行执行）。

- [ ] **Step 6.4: 提交**

```bash
git add docs/bug-fixes-session-loop.md
git commit -m "docs: 记录第二轮循环死锁根本性修复(attempted 自动记账/跨 turn 检测/Abort 真终止)"
```

---

## 三、实现可行性评审

> 按 writing-plans skill 要求逐项推演。结论：**方案可行，无阻塞性风险**。

**7. 签名兼容性**
- `index_message(content, session_id, role, message_index, timestamp_ms)` 签名不变，`session.rs:805-840` 调用点无需改动 ✅
- `LoopDetector::record_tool_call(tool_name, tool_input, output)` 公开签名不变（内部拆出 `record_tool_call_at` 私有版本），`conversation.rs` 两处调用点不变 ✅
- `HookRunResult::cancelled_with_message(String)` / `append_message(String)` 签名不变（`hook_runner` 既有 API，`conversation.rs:1881` 已用）✅
- `notebook::Notebook::load(&Path)` / `save(&Path)` / `append_to_section(&str, &str)` / `set_section(&str, &str)` / `get_section(&str)` 均为既有签名，`append_attempt` 复用 ✅
- `record_turn_failed(usize, &RuntimeError)` / `RuntimeError::new(String)` 既有签名，Task 4/5 复用 ✅

**8. 参数来源**
- `append_attempt(workspace_root, tool_name, effective_input, output)`：四者均在工具循环 Approved 分支作用域内（`self.workspace_root: Option<PathBuf>`、block 的 `tool_name`、`effective_input`、merge 后的 `output`）✅
- `prune_decayed(now_ms, max_age_ms)`：`now_ms` 由 `SystemTime::now()` 计算（conversation.rs:1991 已有同款写法）；`LOOP_DECAY_WINDOW_MS` 为本计划新增常量 ✅
- `record_tool_call_at(..., now_ms)`：私有，仅测试注入 ✅

**9. 数据传递链**
- content_raw：`index_message`（原文入参）→ INSERT 第二列 → `search` `COALESCE(content_raw, content)` → `HistoryHit.content` → `execute_session_search` snippet（conversation.rs:3180）。每层都显式传递，无丢失 ✅
- attempted 记录：工具失败 → `append_attempt` 写 NOTEBOOK 磁盘 → 下一轮 loop 迭代 `conversation.rs:2128` 重新 load 并注入 dynamic_sections → LLM 可见。注入点每轮重建（L2115 `let request = {...}` 在 loop 内），失败后下一轮立即生效 ✅

**10. 判定优先级**
- `record_tool_call_at`：同输入 Abort 优先于同输出（先判 call 再判 output），与原实现一致 ✅
- 工具循环终止点位于 `post_hook_result` merge 之后、`tool_result` 构造之前：确保 hook 消息已回灌、loop_abort_reason 已取走，二者不冲突 ✅
- 漏判 vs 误判：输出规范化剥离时间戳可能把"状态确实变化但仅时间戳不同"的输出误并——但同输入通道（3/6 阈值更低）先行生效，且误并只导致 InjectContext 警告（不终止），成本低；漏判（带时间戳日志循环检测不到）成本高 → 判定方向正确 ✅

**11. retry/重入**
- `append_attempt` 幂等：完全相同的行去重返回 Ok；重复调用成本 = 一次 load/save + 行比对，可忽略 ✅
- 迁移幂等：`history_meta.schema_version` 防重复迁移；v1→v3 后 meta 存在且 version=3，二次 open 不迁移 ✅
- `prune_decayed` 幂等：`retain` 无副作用 ✅

**12. 冲突处理**
- 自动 attempted 记录 vs LLM 手动 `notebook_update` 写同一段：append_attempt 只追加不覆盖，LLM 的 set_section 会整体覆盖（既有语义不变）；无死锁 ✅
- loop abort 返回 Err 与用户中断（`hook_abort_signal`）：互斥路径，各自 return，无冲突 ✅
- 既有测试 `run_turn_errors_when_max_iterations_is_exceeded`（max_iterations=1）与新的提前中止：该测试 1 次迭代即超限，工具调用 1 次 < TOOL_WARN_THRESHOLD(3)，不会触发 loop abort，行为不变 ✅

**13. 与现有系统重叠**
- 检索显示与 `decision_log`：decision_log 用 external content 表（`decisions_fts content='decisions'`，decision_log.rs:146-150），本轮不涉及；其中文检索同样存在 unicode61 不切词问题，属已知限制，留待后续独立评估（计划外）✅
- 自动 attempted 与 `log_decision`：attempted 记"做了什么、失败结果"，decision 记"为什么这样做"，正交（notebook.rs:88 注释已声明）✅
- 跨 turn 检测与 `recovery_orchestrator`：明确不复用（见 Task 5.2 修订注释），避免模拟 executor 误报 Recovered 造成"多跑一轮 + 错误恢复日志" ✅

**14. 失败路径**
- `append_attempt` 失败（NOTEBOOK 损坏/磁盘错误）：调用方 `let _ =` 静默吞错，与历史索引 hook（session.rs:825 注释）一致，不阻断主流程 ✅
- 迁移失败：`open()` 返回 Err，历史索引为 `Option`（session 侧 `Option<Arc<HistoryIndex>>`），降级为"无 session_search"，不阻断会话 ✅
- `prune_decayed` / `record_tool_call` 无 IO，无失败路径 ✅
- loop abort 终止点：`record_turn_failed` 是既有调用（L2085 同款），无新失败路径 ✅

**15. 构造点破坏扫描**
- `ConversationRuntime` 新增字段 `loop_abort_reason: Option<String>`：唯一构造点是 `ConversationRuntime::new()`（conversation.rs:978 附近），需补 `loop_abort_reason: None`。该结构体无 `Default` derive、无 `..Default::default()` 构造（泛型结构体，所有字段由 new 显式设置）；测试均走 `ConversationRuntime::new(...)`，**需同步更新 1 处** ✅
- `LoopDetector` 字段类型变更（u32 → (u32, u64)）：构造点仅 `LoopDetector::new()` = `Default::default()`（derive），测试无字面构造，无需同步 ✅
- `HistoryIndex` 无字段变更，schema/迁移变化不涉及构造点 ✅
- `notebook::append_attempt` 为新增自由函数，无构造点影响 ✅

**16. 成本估算**
- Task 1：约 180 行（schema/迁移/逆变换/测试），含边界（NULL 回退、双重空格、标点）✅
- Task 2：约 70 行（append_attempt + 截断 + 接线 + 测试），含去重与容量边界 ✅
- Task 3：约 160 行（结构体/时间戳注入/衰减/规范化 + 测试），含时间戳启发式边界 ✅
- Task 4：约 120 行（字段/共享方法/两条 hook 重构/终止点 + 测试）✅
- Task 5：约 30 行（错误消息 + 注释修订）✅
- 合计约 560 行 + 5 个提交点；每步均有独立测试可验证，风险可控 ✅

**验证过的关键编译行为**：`matches!(action, LoopAction::Continue)`（按值匹配 + 后续重赋值）在"循环体执行/不执行"两种分支下均编译通过（已用 rustc 实证），Task 3 沿用该模式安全。
