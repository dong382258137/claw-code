# Headroom 核心特性吸收方案

> **状态**: 设计草案
> **日期**: 2026-07-23
> **动机**: 将 Headroom 中最有价值的三个设计模式（Cache Aligner / 内容感知路由 / JSON 压缩）吸收进 claw 架构，保持 Rust 单二进制分发模型，不引入外部运行时依赖。

---

## 0. 现有基础设施盘点

claw 已有远超预期的缓存和压缩基础设施，吸收 Headroom 是在此基础上的精度提升，而非从零构建：

| 现有能力 | 所在模块 | 对标 Headroom | 状态 |
|----------|----------|---------------|------|
| SystemPromptSplit（static/dynamic 分割） | `prompt.rs` | Cache Aligner 结构层 | ✅ 已实现 |
| cache_control ephemeral 标记 | `types.rs`, `openai_compat.rs` | Cache Aligner API 层 | ✅ 已实现 |
| 请求指纹监控 + Cache Break 检测 | `prompt_cache.rs` | Cache Observability | ✅ 已实现 |
| Completion 级缓存（同请求幂等） | `prompt_cache.rs` | — | ✅ 已实现 |
| Microcompact（tool result 摘要） | `compact.rs` | Smart Crusher | ✅ 已实现 |
| Summary Compression（摘要压缩去重） | `summary_compression.rs` | Universal Compressor | ✅ 已实现 |
| CCR 真无损归档 | `tool_result_archive.rs` | CCR Lossless Retrieval | ✅ 1:1 等价 |
| Context Assembler（优先栈 + budget） | `context_assembler.rs` | Context Manager | ✅ 已实现 |
| NOTEBOOK 持久记忆 | `notebook.rs` | Memory System | ✅ 已实现 |

---

## 1. 特性 A：Cache Aligner — 内容级缓存对齐

### 1.1 问题

当前 `SystemPromptSplit` 将 system prompt 分割为 static/dynamic 两区：

```
[static sections]          ← 标 cache_control: ephemeral，可缓存
__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__
[dynamic sections]         ← 不标 cache_control，每次重发
```

但 **static 区内嵌了动态值**，导致即使结构和行为指令不变，static 区的 hash 仍会在跨 turn 时变化：

| 污染源 | 位置 | 影响 |
|--------|------|------|
| `current_date` | `ProjectContext`（现在在 dynamic 区） | ✅ 已正确放在 dynamic |
| `workspace_root` 路径 | environment context | ⚠️ 可能在 static 区 |
| 内存验证 "hints, not facts" | memory 段 | ✅ 本质稳定 |
| 工具定义（含临时 ID/hash） | tools schema | ⚠️ 可能每次变化 |

Headroom 的 Cache Aligner 解决的就是后者——**从本应稳定的内容中提取出动态部分**，用占位符替换或移到动态区尾部。

### 1.2 设计方案

#### 1.2.1 动态值提取器（`DynamicValueExtractor`）

新增模块 `rust/crates/runtime/src/cache_alignment.rs`：

```rust
/// 从文本中识别并提取"动态值"（时间戳、短时效标识、随机 ID 等）。
///
/// 提取后调用方可以选择：
/// - 替换为稳定占位符（保持 hash 不变）→ 缓存在 static 段
/// - 移到 dynamic 段末尾（标注来源，让 LLM 仍可见）
pub struct DynamicValueExtractor {
    /// 正则模式列表，匹配需要提取的动态内容。
    patterns: Vec<Regex>,
}
```

**需要提取的动态模式**（按优先级）：

| 模式 | 示例 | 处理方式 |
|------|------|----------|
| ISO 日期时间 | `2026-07-23T16:28:00` | 替换为 `<current_datetime>` |
| Unix 时间戳（毫秒） | `1784575505000` | 替换为 `<timestamp_ms>` |
| UUID v4 | `550e8400-e29b-...` | 替换为 `<uuid>` |
| 8+ 位随机 hex | `a3f2c8910b7d` | 替换为 `<random_hex>` |
| Session/request ID | （保持原样——本身是稳定标识） | 不处理 |
| 文件绝对路径前缀 | `D:\claw-code-src\...` | 替换为 `<workspace_root>/...` |

#### 1.2.2 SystemPromptBuilder 改造

在 `SystemPromptBuilder::build_split()` 中：

1. **现有流程不变**：static 段 → boundary → dynamic 段
2. **新增步骤**：在返回 `SystemPromptSplit` 前，对所有 static section 调用 `DynamicValueExtractor::extract()`
3. 提取出的动态值组装为一个独立的 `dynamic_extra` section，**追加到 dynamic_sections 末尾**
4. 被替换为占位符的 static sections hash 保持稳定

```rust
// prompt.rs 中 build_split() 的新增逻辑（伪代码）

let mut extractor = DynamicValueExtractor::default();
let cleaned_static: Vec<String> = split.static_sections
    .iter()
    .map(|s| extractor.extract_replace(s))
    .collect();
let extracted_values = extractor.collect_section(); // "Dynamic values: date=<...>, ..."

split.static_sections = cleaned_static;
split.dynamic_sections.push(extracted_values); // 追加到末尾，LLM 仍可见
```

#### 1.2.3 工具定义的稳定性保证

工具定义 JSON schema 是目前最大的缓存污染源——如果工具的 `description` 包含 session 特定信息，整个 tools schema hash 都会变化。

**方案**：确保 `build_tool_definitions()` 中不出现动态值。当前已基本满足，只需做一次审计：
- 检查是否有时间戳、路径、session ID 嵌入工具 description
- 如有，提取为 `{PLACEHOLDER}` 并在每次请求时替换

#### 1.2.4 监控增强

在 `prompt_cache.rs` 已有的 cache break 检测基础上，增加 **按原因的细分统计**：

```rust
pub struct CacheBreakReasons {
    pub model_changed: u64,
    pub system_prompt_changed: u64,     // ← 关注这个：如果 static 区 hash 本不应变却变了
    pub tool_definitions_changed: u64,  // ← 关注这个：同上
    pub message_payload_changed: u64,   // 正常
    pub ttl_expiry: u64,               // 正常
    pub unexpected: u64,               // 已在现有 stats 中
}
```

通过 `claw doctor --cache-stats` 暴露这些指标，让用户可以直观判断缓存效率。

### 1.3 预期收益

- **Anthropic cache_control**：static 区跨 turn 命中率从 ~70% → ~95%
- **OpenAI/DeepSeek 隐式前缀缓存**：前缀稳定后命中率同等提升
- **典型场景**：编码 session 上例行构建时，system prompt + tool defs 约 8K-15K tokens，由 cache_creation 变为 cache_read，单次请求节省 $0.03-$0.05

### 1.4 实现成本

- 新增 `cache_alignment.rs`：~200 行
- `prompt.rs` 改造：~30 行
- `prompt_cache.rs` 统计增强：~40 行
- 总工作量：0.5-1 天

---

## 2. 特性 B：内容感知路由 + JSON 压缩

### 2.1 问题

当前 `microcompact()` 对所有 tool result 使用同一个摘要策略 — `format_tool_result_summary()`（保留前 3 行 + 截断到 240 chars）。这在以下场景严重浪费 token 或丢失信息：

| 场景 | 当前行为 | 理想行为 |
|------|----------|----------|
| `Bash: curl api.example.com` 返回 JSON | 截断到前 3 行 ≈ 无意义片段 | 保留 JSON 结构（keys），压缩 values |
| `Read: large_config.json` | 同上 | 同上 |
| `Grep: 30 文件命中`，每文件 2 行 | 前 3 行只覆盖 1 个文件 | 保留每个文件的首次命中行 |
| 代码文件（`.rs`/`.ts`） | 截断到前 3 行 ≈ 只有 imports | 保留 import + fn/class 签名 |
| 纯文本/日志 | 前 3 行 + "N lines total" | 已较好 |

Headroom 的做法是 ML 内容检测（Magika）+ 类型特定压缩器。claw 不需要 ML，启发式足够。

### 2.2 设计方案

#### 2.2.1 内容类型检测（`ContentType`）

新增 `rust/crates/runtime/src/content_classifier.rs`：

```rust
/// 快速启发式内容分类，不需要 ML。
/// 用前 100 字符做判断，计算成本远低于一次完整的 token 处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    /// JSON（或以 JSON 为主的文本）
    /// 检测：[开头是 `{` 或 `[`] 且整体可解析为 JSON
    Json,
    /// 源代码
    /// 检测：含已知编程语言关键字/模式（fn/class/def/import/package）
    Code(CodeLanguage),
    /// 多条目结构化输出（如 Grep 结果、ls -la 表格）
    /// 检测：≥5 行且每行模式相同
    Tabular,
    /// 普通文本/日志/混合
    Text,
}

pub enum CodeLanguage {
    Rust, TypeScript, Python, Go, Java, Unknown,
}
```

**检测策略**（启发式，<1ms）：

```
fn classify(content: &str) -> ContentType {
    let head = content[..min(100, content.len())];
    let trimmed = head.trim_start();

    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if serde_json::from_str::<Value>(content).is_ok() {
            return ContentType::Json;
        }
    }
    if looks_like_code(head) {
        return ContentType::Code(detect_language(head));
    }
    if is_tabular(content) {
        return ContentType::Tabular;
    }
    ContentType::Text
}
```

#### 2.2.2 类型特定压缩器

在 `compact.rs` 中扩展，`format_tool_result_summary` 变为路由入口：

```rust
fn format_tool_result_summary(tool_name: &str, tool_use_id: &str, output: &str) -> String {
    let ct = classify_content(output);
    match ct {
        ContentType::Json => format_json_summary(tool_name, tool_use_id, output),
        ContentType::Code(lang) => format_code_summary(tool_name, tool_use_id, output, lang),
        ContentType::Tabular => format_tabular_summary(tool_name, tool_use_id, output),
        ContentType::Text => format_text_summary(tool_name, tool_use_id, output), // 现有逻辑
    }
}
```

##### JSON 压缩器

**策略**：保留完整 JSON 结构，只压缩叶子值。

```
输入（2000 chars）:
{"users":[{"id":1,"name":"Alice","email":"alice@example.com","bio":"Likes Rust...500 chars..."},
          {"id":2,"name":"Bob","email":"bob@example.com","bio":"Likes Python...500 chars..."}]}

输出（~300 chars）:
[Bash output: JSON object, 2000 chars → {"users":[{"id":1,"name":"Alice","email":"alice@example.com","bio":"Likes Rust…"},…]}  (2 items, keys: id, name, email, bio) … use recall_full with tool_use_id=call_xxx to retrieve full output…]
```

**算法**：
1. 解析 JSON → `serde_json::Value`
2. 递归遍历，保留所有 key 名和结构
3. 对每个 string value：截断到 80 chars
4. 对数组：[first_item, …, last_item]，中间用 `…` 省略（最多保留 3 个元素）
5. 对 object：保留所有 key，value 同上
6. 附加统计：`(3 items, 12 keys)`

```rust
fn compress_json_value(value: &Value, budget: usize) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), compress_json_value(v, budget / map.len().max(1)));
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            if arr.len() <= 3 {
                Value::Array(arr.iter().map(|v| compress_json_value(v, budget)).collect())
            } else {
                let mut compacted = vec![
                    compress_json_value(&arr[0], budget),
                    Value::String("…".to_string()),
                    compress_json_value(&arr[arr.len()-1], budget),
                ];
                Value::Array(compacted)
            }
        }
        Value::String(s) => {
            if s.chars().count() > 80 {
                Value::String(format!("{}…", s.chars().take(79).collect::<String>()))
            } else {
                value.clone()
            }
        }
        _ => value.clone(),
    }
}
```

##### Code 压缩器

**策略**：保留结构签名，折叠实现体。

```
输入（500 行 Rust 文件）:
//! Module doc comment
use std::collections::HashMap;
pub fn foo(x: i32) -> i32 { ... }
impl MyStruct { pub fn bar() { ... } pub fn baz() { ... } }

输出:
[Read output summarized: 8000 chars → Rust source, 500 lines total.
  //! Module doc comment
  use std::collections::HashMap;
  pub fn foo(x: i32) -> i32 { … }
  impl MyStruct { pub fn bar() { … } pub fn baz() { … } }
  … use recall_full with tool_use_id=call_xxx]
```

**算法**：
1. 按行扫描，识别以下模式并保留：
   - `//!` / `///` doc comments（只保留第一行）
   - `use ...`（保留所有）
   - `pub fn / fn / pub async fn`（签名 + `{ … }`）
   - `impl ... {` + 方法签名（保留签名，折叠 body）
   - `struct / enum / trait` 定义
   - `const / static` 定义
2. 所有其他行（实现体、内部逻辑）→ 省略
3. 附加语言标签和总行数

##### Tabular 压缩器

**策略**：保留表头 + 每列的首次出现 + "代表性"行。

```
输入:
apples  3  red   0.50
bananas 5  yellow 0.30
cherries 2 red  0.80
dates   10 brown 0.60

输出:
[tabular: 4 rows × 4 cols, header detected
  apples  3  red   0.50
  bananas 5  yellow 0.30
  … 2 more rows …
]
```

#### 2.2.3 集成到 Microcompact

现有 `microcompact()` 入口不变。变更仅在 `format_tool_result_summary()` 内部路由：

```
microcompact()  →  对旧 tool result 迭代
                  →  检测内容类型 (JSON / Code / Tabular / Text)
                  →  调用对应压缩器
                  →  生成摘要 placeholder
                  →  archiver() 写入 ToolResultArchive（不变）
```

#### 2.2.4 `is_already_summarized` 兼容

现有的 `is_already_summarized()` 检测逻辑需要更新以识别新格式的摘要：

```rust
fn is_already_summarized(output: &str) -> bool {
    // 旧格式兼容
    if output.starts_with('[')
        && output.contains(" output summarized: ")
        && output.ends_with("…]")
        && output.contains(" chars → ")
    {
        return true;
    }
    // 新 JSON 格式
    if output.starts_with('[')
        && output.contains(": JSON ")
        && output.contains(" keys: ")
    {
        return true;
    }
    // 新 Code 格式
    if output.starts_with('[')
        && output.contains(" source, ")
        && output.contains(" lines total")
    {
        return true;
    }
    false
}
```

### 2.3 预期收益

| 内容类型 | 当前压缩率 | 设计压缩率 | 额外信息保留 |
|----------|------------|------------|-------------|
| JSON API 响应 | ~70%（前 3 行常无意义） | 60-90%（结构完整，值压缩） | 所有 key 名、数组轮廓 |
| 大代码文件 | ~95%（只留 import） | 85-92%（签名保留） | 函数签名、类型定义 |
| Grep 多文件结果 | ~80%（只第一个文件） | 50-80%（每文件首命中） | 覆盖范围可见 |
| 纯文本/日志 | ~80% | ~80%（不变） | — |

### 2.4 实现成本

- 新增 `content_classifier.rs`：~150 行
- 新增 `content_compression.rs`（JSON/Code/Tabular 压缩器）：~300 行
- `compact.rs` 改造（路由 + `is_already_summarized` 更新）：~40 行
- 测试：~200 行
- 总工作量：1-2 天

---

## 3. 实现路线图

### Phase 1：Cache Aligner（优先级 P0）

```
day 1  新增 cache_alignment.rs
       ↓
       改造 SystemPromptBuilder::build_split()
       ↓
       添加 cache_break 细分统计
       ↓
       编写测试 + 手动验证缓存命中率提升
```

**验收标准**：
- `claw doctor --cache-stats` 显示 static 区 break 事件中 `system_prompt_changed` 占比 < 5%
- Anthropic 请求中 `cache_read_input_tokens` 占总 input 的 stable 部分比例稳定

### Phase 2：内容感知路由（优先级 P0）

```
day 2  新增 content_classifier.rs + 测试
       ↓
       day 2-3  新增 content_compression.rs（JSON + Code + Tabular）
       ↓
       改造 compact.rs 的路由 + is_already_summarized
       ↓
       编写测试（每种类型 + 边界条件）
```

**验收标准**：
- `Read large_config.json` 的摘要包含完整 JSON 结构而非前 3 行无意义片段
- `Read src/main.rs` 的摘要保留 `fn` 签名
- 所有现有 `microcompact` 测试继续通过
- `cargo test --workspace` 全绿

### Phase 3：性能验证（可选）

```
day 4  benchmark: microcompact 吞吐量（不应退化 > 5%）
       ↓
       手动验证：真实编码 session 中 token 节省量
       ↓
       微调参数（MAX_PREVIEW_ITEMS 等）
```

---

## 4. 不做的事情（明确边界）

| Headroom 特性 | 不做的原因 |
|---------------|-----------|
| Magika ML 内容检测 | 引入 ONNX/Python 依赖，违背纯 Rust 单二进制模型；启发式已够用 |
| 独立 Proxy 模式 | claw 是嵌入式 runtime，不适用独立中间件部署模式 |
| Smart Crusher 的 embedding 相关性 | 嵌入模型引入额外计算成本，与 claw 的本地优先、低延迟定位矛盾 |
| Image 压缩 | claw 当前不处理图片输入 |
| 多 Agent 共享上下文缓存 | 已在现有 subagent 设计中解决（独立 LLM 请求 + 独立缓存） |

---

## 5. 架构决策记录

| 决策 | 理由 |
|------|------|
| 不引入 Headroom 为 Python 依赖 | 破坏 claw 的单二进制部署模型；增加 DevOps 复杂度 |
| 使用启发式而非 ML 做内容检测 | 前 100 字符 + JSON 解析器已能覆盖 >95% 场景；ML 收益不成比例 |
| 压缩器嵌入现有 `compact.rs` 流程 | 最小化接口变更，保持 `microcompact` 的调用语义不变 |
| 动态值提取放在 `build_split` 中 | 避免在所有 static section 来源处分散处理；集中提取更容易审计 |
| CCR 机制保持不变 | 现有 `ToolResultArchive` 已经是 1:1 功能等价物，无需改动 |
