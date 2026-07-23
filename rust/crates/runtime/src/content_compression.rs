//! 类型特定压缩器,用于内容感知路由(microcompact Phase 2)。
//!
//! 与 `content_classifier.rs` 配合:每个 `ContentType` 对应一个压缩器。
//! 所有压缩器输出统一格式的 placeholder,以便 `is_already_summarized` 识别。
//!
//! 统一输出格式:
//! ```text
//! [{tool_name} {content_type} summarized: {original_len} chars → {preview}{stats}… use recall_full with tool_use_id={tool_use_id} to retrieve full output…]
//! ```
//!
//! 其中 `{content_type}` ∈ {JSON, Code, Tabular, Text},让 `is_already_summarized`
//! 能通过子串匹配区分新旧格式。
//!
//! 详见 `docs/design-headroom-absorption.md` 第 2 节。

use crate::content_classifier::{classify, CodeLanguage, ContentType};
use serde_json::{Map, Value};

/// 字符串值截断阈值。超过此长度的 string 叶子值会被截断。
const MAX_STRING_VALUE_CHARS: usize = 80;
/// 数组保留的最大元素数(首尾各一,中间省略)。
const MAX_ARRAY_KEEP: usize = 3;

/// 入口:根据内容类型路由到对应压缩器。
///
/// 被 `compact.rs::format_tool_result_summary` 调用,替代原来的
/// "前 3 行 + 240 chars"单一逻辑。
#[must_use]
pub fn format_summary(tool_name: &str, tool_use_id: &str, output: &str) -> String {
    let content_type = classify(output);
    match content_type {
        ContentType::Json => format_json_summary(tool_name, tool_use_id, output),
        ContentType::Code(lang) => format_code_summary(tool_name, tool_use_id, output, lang),
        ContentType::Log => format_log_summary(tool_name, tool_use_id, output),
        ContentType::Tabular => format_tabular_summary(tool_name, tool_use_id, output),
        ContentType::Text => format_text_summary(tool_name, tool_use_id, output),
    }
}

// ============================================================================
// JSON 压缩器
// ============================================================================

/// JSON 压缩器:保留完整 JSON 结构,只压缩叶子值。
///
/// 输出示例:
/// ```text
/// [Bash JSON summarized: 2000 chars → {"users":[{"id":1,"name":"Alice","bio":"Likes Rust…"},…]} (2 items, keys: id, name, bio) … use recall_full with tool_use_id=call_xxx to retrieve full output…]
/// ```
#[must_use]
pub fn format_json_summary(tool_name: &str, tool_use_id: &str, output: &str) -> String {
    let original_len = output.chars().count();
    let parsed: Value = match serde_json::from_str(output.trim()) {
        Ok(v) => v,
        Err(_) => {
            // 解析失败,回退到 Text 压缩器(不应发生,classify 已验证可解析)
            return format_text_summary(tool_name, tool_use_id, output);
        }
    };
    let compressed = compress_json_value(&parsed);
    let preview = serde_json::to_string(&compressed).unwrap_or_else(|_| "{}".to_string());
    let stats = json_stats(&parsed);
    format!(
        "[{tool_name} JSON summarized: {original_len} chars → {preview} {stats}… use recall_full with tool_use_id={tool_use_id} to retrieve full output…]"
    )
}

/// 递归压缩 JSON Value:保留所有 key 名和结构,只压缩叶子值。
///
/// - string value:超过 `MAX_STRING_VALUE_CHARS` 截断加 `…`
/// - array:>3 个元素时保留首尾,中间用 `"__omitted_N__"` 标记省略数量
/// - object:保留所有 key,value 递归压缩
/// - number/bool/null:原样保留
fn compress_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), compress_json_value(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            if arr.len() <= MAX_ARRAY_KEEP {
                Value::Array(arr.iter().map(compress_json_value).collect())
            } else {
                let omitted = arr.len() - 2;
                let mut compacted = Vec::with_capacity(3);
                compacted.push(compress_json_value(&arr[0]));
                compacted.push(Value::String(format!("__omitted_{omitted}__")));
                compacted.push(compress_json_value(&arr[arr.len() - 1]));
                Value::Array(compacted)
            }
        }
        Value::String(s) => {
            let char_count = s.chars().count();
            if char_count > MAX_STRING_VALUE_CHARS {
                let truncated: String = s.chars().take(MAX_STRING_VALUE_CHARS - 1).collect();
                Value::String(format!("{truncated}…"))
            } else {
                value.clone()
            }
        }
        _ => value.clone(),
    }
}

/// 生成 JSON 统计信息字符串,如 `(2 items, keys: id, name, bio)`。
///
/// 对于 object,如果其值包含 array,优先报告 array 第一个元素的 keys
/// (这通常是 API 响应的主要数据结构);否则报告 object 自己的 keys。
fn json_stats(value: &Value) -> String {
    match value {
        Value::Array(arr) => {
            let count = arr.len();
            let keys = arr.first().map(collect_keys).unwrap_or_default();
            if keys.is_empty() {
                format!("({count} items)")
            } else {
                format!("({count} items, keys: {})", keys.join(", "))
            }
        }
        Value::Object(map) => {
            // 若 object 的某个值是 array,报告该 array 首元素的 keys(更实用)
            for v in map.values() {
                if let Value::Array(arr) = v {
                    if let Some(first) = arr.first() {
                        let keys = collect_keys(first);
                        if !keys.is_empty() {
                            let count = arr.len();
                            let obj_keys: Vec<&str> = map.keys().map(String::as_str).collect();
                            return format!(
                                "({count} items under {}, keys: {})",
                                obj_keys.join(", "),
                                keys.join(", ")
                            );
                        }
                    }
                }
            }
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            format!("(1 object, keys: {})", keys.join(", "))
        }
        _ => String::new(),
    }
}

/// 收集 JSON object 顶层的 key 名(按出现顺序,去重)。
fn collect_keys(value: &Value) -> Vec<String> {
    if let Value::Object(map) = value {
        map.keys().cloned().collect()
    } else {
        Vec::new()
    }
}

// ============================================================================
// Code 压缩器
// ============================================================================

/// Code 压缩器:保留结构签名,折叠实现体。
///
/// 输出示例:
/// ```text
/// [Read Code summarized: 8000 chars → Rust source, 500 lines total.
///   //! Module doc comment
///   use std::collections::HashMap;
///   pub fn foo(x: i32) -> i32 { … }
///   impl MyStruct { pub fn bar() { … } }
///   … use recall_full with tool_use_id=call_xxx to retrieve full output…]
/// ```
#[must_use]
pub fn format_code_summary(
    tool_name: &str,
    tool_use_id: &str,
    output: &str,
    lang: CodeLanguage,
) -> String {
    let original_len = output.chars().count();
    let total_lines = output.lines().count();
    let lang_label = language_label(lang);
    let signature = extract_code_signatures(output);
    if signature.is_empty() {
        // 无签名命中,回退到 Text 压缩器
        return format_text_summary(tool_name, tool_use_id, output);
    }
    format!(
        "[{tool_name} Code summarized: {original_len} chars → {lang_label} source, {total_lines} lines total.\n  {signature}\n  … use recall_full with tool_use_id={tool_use_id} to retrieve full output…]"
    )
}

/// 返回语言的显示标签。
fn language_label(lang: CodeLanguage) -> &'static str {
    match lang {
        CodeLanguage::Rust => "Rust",
        CodeLanguage::TypeScript => "TypeScript",
        CodeLanguage::Python => "Python",
        CodeLanguage::Go => "Go",
        CodeLanguage::Java => "Java",
        CodeLanguage::Unknown => "Unknown",
    }
}

/// 提取代码的结构签名。
///
/// 保留以下行(每类最多保留前 5 行,避免签名过长):
/// - `//!` / `///` doc 注释(只保留第一行)
/// - `use ` / `import ` / `package ` 声明
/// - `pub fn` / `fn ` / `def ` / `func ` / `function ` 函数签名
/// - `impl ` / `struct ` / `enum ` / `trait ` / `class ` / `interface ` 类型定义
/// - `const ` / `static ` / `let ` 顶层声明
///
/// 函数体和实现细节用 `{ … }` 折叠。
fn extract_code_signatures(content: &str) -> String {
    let mut signatures: Vec<String> = Vec::new();
    let mut doc_seen = false;
    let mut use_count = 0;
    let mut fn_count = 0;
    let mut type_count = 0;
    let mut const_count = 0;
    const MAX_PER_CATEGORY: usize = 5;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // doc 注释:只保留第一个
        if (trimmed.starts_with("//!") || trimmed.starts_with("///")) && !doc_seen {
            signatures.push(trimmed.to_string());
            doc_seen = true;
            continue;
        }
        // use/import/package
        if (trimmed.starts_with("use ")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("package "))
            && use_count < MAX_PER_CATEGORY
        {
            signatures.push(fold_inline_block(trimmed));
            use_count += 1;
            continue;
        }
        // 函数签名
        if (trimmed.starts_with("pub fn")
            || trimmed.starts_with("fn ")
            || trimmed.starts_with("def ")
            || trimmed.starts_with("func ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("async fn")
            || trimmed.starts_with("pub async fn"))
            && fn_count < MAX_PER_CATEGORY
        {
            signatures.push(fold_or_append_block(trimmed));
            fn_count += 1;
            continue;
        }
        // 类型定义
        if (trimmed.starts_with("impl ")
            || trimmed.starts_with("struct ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("trait ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("interface "))
            && type_count < MAX_PER_CATEGORY
        {
            signatures.push(fold_or_append_block(trimmed));
            type_count += 1;
            continue;
        }
        // 顶层声明
        if (trimmed.starts_with("const ")
            || trimmed.starts_with("static ")
            || trimmed.starts_with("let "))
            && const_count < MAX_PER_CATEGORY
        {
            signatures.push(fold_inline_block(trimmed));
            const_count += 1;
            continue;
        }
    }
    signatures.join("\n  ")
}

/// 折叠行内的 `{ ... }` 块为 `{ … }`,只保留签名部分。
///
/// 例如 `pub fn foo(x: i32) -> i32 { x + 1 }` → `pub fn foo(x: i32) -> i32 { … }`
///
/// 保留 `{` 前的空白(通常是单个空格),让输出格式与源代码一致。
fn fold_inline_block(line: &str) -> String {
    if let Some(open) = line.find('{') {
        let before = &line[..open];
        format!("{before}{{ … }}")
    } else {
        line.to_string()
    }
}

/// 对函数/类型签名行:若行内已有 `{` 则折叠实现体,否则追加 `{ … }`。
///
/// - `fn foo(x: i32) -> i32 { x + 1 }` → `fn foo(x: i32) -> i32 { … }`(折叠)
/// - `fn foo(x: i32) -> i32 {` → `fn foo(x: i32) -> i32 { … }`(折叠空块)
/// - `fn foo(x: i32) -> i32`(无 `{`)→ `fn foo(x: i32) -> i32 { … }`(追加)
fn fold_or_append_block(line: &str) -> String {
    if line.contains('{') {
        fold_inline_block(line)
    } else {
        format!("{line} {{ … }}")
    }
}

// ============================================================================
// Tabular 压缩器
// ============================================================================

/// Tabular 压缩器:保留表头 + 前几行代表性行。
///
/// 输出示例:
/// ```text
/// [Grep Tabular summarized: 5000 chars → 30 rows × 4 cols, header detected
///   name | age | city
///   Alice | 30 | NYC
///   Bob | 25 | LA
///   … 28 more rows …
///   … use recall_full with tool_use_id=call_xxx to retrieve full output…]
/// ```
#[must_use]
pub fn format_tabular_summary(tool_name: &str, tool_use_id: &str, output: &str) -> String {
    let original_len = output.chars().count();
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    let total_rows = lines.len();
    if total_rows == 0 {
        return format_text_summary(tool_name, tool_use_id, output);
    }
    // 检测列数(用第一行)
    let cols = detect_column_count(lines[0]);
    let header_detected = is_likely_header(lines[0]);
    // 保留前 3 行作为预览
    const PREVIEW_ROWS: usize = 3;
    let preview_lines: Vec<&str> = lines.iter().take(PREVIEW_ROWS).copied().collect();
    let preview = preview_lines.join("\n  ");
    let omitted = total_rows.saturating_sub(PREVIEW_ROWS);
    let omitted_info = if omitted > 0 {
        format!("\n  … {omitted} more rows …")
    } else {
        String::new()
    };
    let header_info = if header_detected { ", header detected" } else { "" };
    let cols_info = if cols > 0 {
        format!(" × {cols} cols{header_info}")
    } else {
        header_info.to_string()
    };
    format!(
        "[{tool_name} Tabular summarized: {original_len} chars → {total_rows} rows{cols_info}\n  {preview}{omitted_info}\n  … use recall_full with tool_use_id={tool_use_id} to retrieve full output…]"
    )
}

/// 检测行的列数。
fn detect_column_count(line: &str) -> usize {
    if line.contains('|') {
        line.split('|').count()
    } else if line.contains('\t') {
        line.split('\t').count()
    } else if line.contains("   ") {
        line.split_whitespace().count()
    } else {
        0
    }
}

/// 启发式判断是否为表头行(第一行)。
///
/// 简单启发式:如果行包含字母且不含纯数字 token,可能是表头。
fn is_likely_header(line: &str) -> bool {
    let tokens: Vec<&str> = if line.contains('|') {
        line.split('|').map(|s| s.trim()).collect()
    } else {
        line.split_whitespace().collect()
    };
    if tokens.is_empty() {
        return false;
    }
    // 所有 token 都不是纯数字 → 可能是表头
    tokens.iter().all(|t| !t.is_empty() && t.parse::<f64>().is_err())
}

// ============================================================================
// Text 压缩器(原逻辑)
// ============================================================================

/// Text 压缩器:保留前 3 行 + 截断到 240 chars。
///
/// 这是原 `format_tool_result_summary` 的逻辑,保留作为兜底。
/// 输出格式与原格式兼容(以 `[` 开头,`…]` 结尾),但 content_type 标记为 `Text`
/// 以便 `is_already_summarized` 识别新格式。
#[must_use]
pub fn format_text_summary(tool_name: &str, tool_use_id: &str, output: &str) -> String {
    let original_len = output.chars().count();
    let total_lines = output.lines().count();

    const MAX_PREVIEW_LINES: usize = 3;
    const MAX_PREVIEW_CHARS: usize = 240;

    let preview_lines: Vec<&str> = output.lines().take(MAX_PREVIEW_LINES).collect();
    let mut preview = preview_lines.join("\n");

    if preview.chars().count() > MAX_PREVIEW_CHARS {
        let truncated: String = preview.chars().take(MAX_PREVIEW_CHARS).collect();
        preview = format!("{truncated}…");
    }

    let line_info = if total_lines > MAX_PREVIEW_LINES {
        format!(" ({total_lines} lines total)")
    } else {
        String::new()
    };

    format!(
        "[{tool_name} Text summarized: {original_len} chars → {preview}{line_info}… use recall_full with tool_use_id={tool_use_id} to retrieve full output…]"
    )
}

// ============================================================================
// Log 压缩器(Headroom LogCompressor 对标)
// ============================================================================

/// 保留的重要日志级别(大小写不敏感匹配,词边界)。
const LOG_CRITICAL_LEVELS: &[&str] = &["ERROR", "FATAL", "PANIC", "FAILED"];
const LOG_WARNING_LEVELS: &[&str] = &["WARN", "WARNING"];

/// 每种"重复模式"最多保留的首次出现行数。超过此阈值的相似行折叠为计数摘要。
const MAX_REPEATED_PATTERN_KEEP: usize = 3;
/// 保留的尾部行数(结果摘要通常在末尾)。
const LOG_TAIL_LINES: usize = 5;
/// 压缩后预览的最大字符数,避免摘要本身过长。
const LOG_PREVIEW_MAX_CHARS: usize = 800;

/// Log 压缩器:保留 ERROR/WARN + 首次出现模式 + 尾部摘要。
///
/// 策略(对标 Headroom LogCompressor,压缩率 80-94%):
/// 1. 保留所有 ERROR/FATAL/PANIC/FAILED 行(硬约束,不经过折叠)
/// 2. 保留所有 WARN/WARNING 行
/// 3. 对其他行按"模式"分组,每组保留前 3 行 + 折叠计数
/// 4. 保留最后 5 行(构建/测试结果摘要通常在末尾)
/// 5. 去重合并,保持原始顺序
///
/// 输出示例:
/// ```text
/// [Bash Log summarized: 5000 chars → [ERROR] connection refused
/// [WARN] slow query 1.2s
/// Compiling proc-macro2 v1.0.81
/// Compiling unicode-ident v1.0.12
/// Compiling libc v0.2.153
/// … 47 similar Compiling lines …
///     Finished release [optimized] target(s) in 42.88s (5 critical, 1 warning, 50 info, 12 patterns)… use recall_full with tool_use_id=call_xxx to retrieve full output…]
/// ```
#[must_use]
pub fn format_log_summary(tool_name: &str, tool_use_id: &str, output: &str) -> String {
    let original_len = output.chars().count();
    let lines: Vec<&str> = output.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        return format_text_summary(tool_name, tool_use_id, output);
    }

    // 1. 分类:critical / warning / other
    let mut critical_lines: Vec<(usize, &str)> = Vec::new(); // (原行号, 行内容)
    let mut warning_lines: Vec<(usize, &str)> = Vec::new();
    let mut other_lines: Vec<(usize, &str)> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let upper = line.to_uppercase();
        if matches_log_level(&upper, LOG_CRITICAL_LEVELS) {
            critical_lines.push((idx, line));
        } else if matches_log_level(&upper, LOG_WARNING_LEVELS) {
            warning_lines.push((idx, line));
        } else {
            other_lines.push((idx, line));
        }
    }

    // 2. 对 other_lines 按模式分组 + 折叠
    let mut folded_others: Vec<(usize, String)> = Vec::new();
    let mut pattern_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut current_pattern: Option<String> = None;
    let mut current_count = 0usize;
    let mut current_first_idx = 0usize;

    for (idx, line) in &other_lines {
        let pattern = extract_log_pattern(line);
        if Some(&pattern) == current_pattern.as_ref() {
            current_count += 1;
        } else {
            // flush previous pattern
            if current_count > 0 {
                if let Some(ref p) = current_pattern {
                    pattern_counts.entry(p.clone()).or_insert(0);
                    *pattern_counts.get_mut(p).unwrap() += current_count;
                }
                let keep = current_count.min(MAX_REPEATED_PATTERN_KEEP);
                for i in 0..keep {
                    let src_idx = current_first_idx + i;
                    if src_idx < other_lines.len() {
                        folded_others.push((other_lines[src_idx].0, other_lines[src_idx].1.to_string()));
                    }
                }
                if current_count > keep {
                    let omitted = current_count - keep;
                    let last_kept_idx = folded_others.last().map(|(i, _)| *i).unwrap_or(0);
                    let p = current_pattern.as_deref().unwrap_or("unknown");
                    folded_others.push((last_kept_idx, format!("… {omitted} similar {p} lines …")));
                }
            }
            current_pattern = Some(pattern);
            current_count = 1;
            current_first_idx = other_lines.iter().position(|(i, _)| i == idx).unwrap_or(0);
        }
    }
    // flush last pattern
    if current_count > 0 {
        if let Some(ref p) = current_pattern {
            *pattern_counts.entry(p.clone()).or_insert(0) += current_count;
        }
        let keep = current_count.min(MAX_REPEATED_PATTERN_KEEP);
        for i in 0..keep {
            let src_idx = current_first_idx + i;
            if src_idx < other_lines.len() {
                folded_others.push((other_lines[src_idx].0, other_lines[src_idx].1.to_string()));
            }
        }
        if current_count > keep {
            let omitted = current_count - keep;
            let last_kept_idx = folded_others.last().map(|(i, _)| *i).unwrap_or(0);
            let p = current_pattern.as_deref().unwrap_or("unknown");
            folded_others.push((last_kept_idx, format!("… {omitted} similar {p} lines …")));
        }
    }

    // 3. 合并所有保留行 + 按原行号排序 + 去重
    let mut kept: Vec<(usize, String)> = Vec::new();
    kept.extend(critical_lines.iter().map(|(i, l)| (*i, l.to_string())));
    kept.extend(warning_lines.iter().map(|(i, l)| (*i, l.to_string())));
    kept.extend(folded_others.iter().cloned());

    // 4. 追加尾部行(结果摘要)
    let tail_start = total_lines.saturating_sub(LOG_TAIL_LINES);
    for (idx, line) in lines.iter().enumerate() {
        if idx >= tail_start {
            kept.push((idx, line.to_string()));
        }
    }

    // 按原行号排序 + 去重(折叠消息不参与去重,因为它们与最后保留行共享索引)
    kept.sort_by_key(|(i, _)| *i);
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    kept.retain(|(idx, s)| {
        let is_fold_message = s.contains(" similar ");
        is_fold_message || seen.insert(*idx)
    });

    // 5. 组装预览,截断到 LOG_PREVIEW_MAX_CHARS
    let preview_lines: Vec<String> = kept.iter().map(|(_, s)| s.clone()).collect();
    let mut preview = preview_lines.join("\n");
    if preview.chars().count() > LOG_PREVIEW_MAX_CHARS {
        let truncated: String = preview.chars().take(LOG_PREVIEW_MAX_CHARS).collect();
        preview = format!("{truncated}…");
    }

    // 6. 统计信息
    let critical_count = critical_lines.len();
    let warning_count = warning_lines.len();
    let info_count = other_lines.len();
    let pattern_count = pattern_counts.len();
    let stats = format!(
        "({critical_count} critical, {warning_count} warning, {info_count} info, {pattern_count} patterns)"
    );

    format!(
        "[{tool_name} Log summarized: {original_len} chars → {preview} {stats}… use recall_full with tool_use_id={tool_use_id} to retrieve full output…]"
    )
}

/// 检查日志行是否包含指定的级别关键字(词边界匹配)。
fn matches_log_level(upper_line: &str, levels: &[&str]) -> bool {
    levels.iter().any(|level| {
        if let Some(idx) = upper_line.find(level) {
            let before_ok = idx == 0
                || !upper_line.as_bytes().get(idx - 1).is_some_and(|c| c.is_ascii_alphabetic());
            let after_idx = idx + level.len();
            let after_ok = after_idx >= upper_line.len()
                || !upper_line.as_bytes().get(after_idx).is_some_and(|c| c.is_ascii_alphabetic());
            before_ok && after_ok
        } else {
            false
        }
    })
}

/// 提取日志行的"模式签名"用于重复折叠。
///
/// 规则:取行首第一个词作为模式(如 "Compiling"、"Finished"、"test"、"[2026-07-23")。
/// 同模式的连续行折叠为 "N similar X lines"。
fn extract_log_pattern(line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return "blank".to_string();
    }
    // 取第一个 token(到空格或 `[`/`]` 边界)
    let first_token: String = trimmed
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    // 如果第一个 token 是时间戳类(以 `[` 开头),用第二个 token 作模式
    if first_token.starts_with('[') {
        let after_bracket = trimmed
            .find(']')
            .map(|i| trimmed[i + 1..].trim_start())
            .unwrap_or(trimmed);
        let second_token: String = after_bracket
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect();
        if second_token.is_empty() {
            first_token
        } else {
            second_token
        }
    } else {
        first_token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- JSON 压缩器测试 ----

    #[test]
    fn json_summary_preserves_structure() {
        // bio 超过 80 字符,验证截断
        let long_bio = "Likes Rust and writes very long biographies that easily exceed the eighty character limit for testing";
        assert!(long_bio.chars().count() > 80, "test bio must exceed 80 chars");
        let input = format!(r#"{{"name":"Alice","bio":"{long_bio}"}}"#);
        let result = format_json_summary("Bash", "call_1", &input);
        assert!(result.starts_with("[Bash JSON summarized:"));
        assert!(result.contains("\"name\":\"Alice\""));
        // bio 应被截断到 80 字符(79 + …)
        assert!(
            result.contains("…"),
            "truncated bio should contain ellipsis: {result}"
        );
        assert!(result.contains("use recall_full with tool_use_id=call_1"));
    }

    #[test]
    fn json_summary_omits_middle_array_elements() {
        let input = r#"[1,2,3,4,5,6,7,8,9,10]"#;
        let result = format_json_summary("Bash", "call_2", input);
        assert!(result.contains("__omitted_8__"));
        assert!(result.contains("(10 items"));
    }

    #[test]
    fn json_summary_keeps_short_arrays_intact() {
        let input = r#"[1,2,3]"#;
        let result = format_json_summary("Bash", "call_3", input);
        assert!(!result.contains("__omitted_"));
        assert!(result.contains("[1,2,3]"));
    }

    #[test]
    fn json_summary_reports_keys() {
        let input = r#"{"users":[{"id":1,"name":"Alice"}]}"#;
        let result = format_json_summary("Bash", "call_4", input);
        assert!(result.contains("keys: id, name"));
    }

    // ---- Code 压缩器测试 ----

    #[test]
    fn code_summary_preserves_rust_signatures() {
        let input = "//! Module doc\nuse std::collections::HashMap;\npub fn foo(x: i32) -> i32 {\n    x + 1\n}\nimpl Bar {\n    pub fn baz() {}\n}\n";
        let result = format_code_summary("Read", "call_5", input, CodeLanguage::Rust);
        assert!(result.starts_with("[Read Code summarized:"));
        assert!(result.contains("Rust source"));
        assert!(result.contains("//! Module doc"));
        assert!(result.contains("use std::collections::HashMap;"));
        assert!(result.contains("pub fn foo(x: i32) -> i32 { … }"));
        assert!(result.contains("impl Bar { … }"));
        assert!(result.contains("use recall_full with tool_use_id=call_5"));
    }

    #[test]
    fn code_summary_folds_inline_blocks() {
        let input = "fn add(a: i32, b: i32) -> i32 { a + b }\n";
        let result = format_code_summary("Read", "call_6", input, CodeLanguage::Rust);
        assert!(result.contains("fn add(a: i32, b: i32) -> i32 { … }"));
        assert!(!result.contains("a + b"));
    }

    #[test]
    fn code_summary_caps_signatures_per_category() {
        // 10 个 fn 定义,应只保留前 5 个
        let mut input = String::new();
        for i in 0..10 {
            input.push_str(&format!("fn f{i}(x: i32) -> i32 {{ x + {i} }}\n"));
        }
        let result = format_code_summary("Read", "call_7", &input, CodeLanguage::Rust);
        assert!(result.contains("fn f0(x: i32) -> i32 { … }"));
        assert!(result.contains("fn f4(x: i32) -> i32 { … }"));
        assert!(!result.contains("fn f5"));
    }

    // ---- Tabular 压缩器测试 ----

    #[test]
    fn tabular_summary_preserves_header_and_preview() {
        let input = "name | age\n---- | ---\nAlice | 30\nBob | 25\nCarol | 40\nDave | 35\nEve | 28\n";
        let result = format_tabular_summary("Grep", "call_8", input);
        assert!(result.starts_with("[Grep Tabular summarized:"));
        assert!(result.contains("rows"));
        assert!(result.contains("cols"));
        assert!(result.contains("name | age"));
        assert!(result.contains("Alice | 30"));
        assert!(result.contains("more rows"));
        assert!(result.contains("use recall_full with tool_use_id=call_8"));
    }

    #[test]
    fn tabular_summary_no_omitted_info_for_short_tables() {
        let input = "a 1\nb 2\nc 3\nd 4\ne 5\n";
        let result = format_tabular_summary("Grep", "call_9", input);
        // 5 行,保留前 3 行,omitted = 2 > 0,所以应该有 more rows
        assert!(result.contains("more rows"));
    }

    // ---- Text 压缩器测试 ----

    #[test]
    fn text_summary_preserves_first_three_lines() {
        let input = "line one\nline two\nline three\nline four\nline five\n";
        let result = format_text_summary("Read", "call_a", input);
        assert!(result.starts_with("[Read Text summarized:"));
        assert!(result.contains("line one"));
        assert!(result.contains("line two"));
        assert!(result.contains("line three"));
        assert!(!result.contains("line four"));
        assert!(result.contains("(5 lines total)"));
        assert!(result.contains("use recall_full with tool_use_id=call_a"));
    }

    #[test]
    fn text_summary_truncates_long_preview() {
        let long_line: String = "a".repeat(300);
        let input = format!("{long_line}\n");
        let result = format_text_summary("Read", "call_b", &input);
        // 应截断到 240 chars 加 …
        assert!(result.contains('…'));
        // 不应包含完整的 300 个 a
        assert!(!result.contains(&"a".repeat(300)));
    }

    #[test]
    fn text_summary_omits_line_count_for_short_output() {
        let input = "only one line\n";
        let result = format_text_summary("Read", "call_c", input);
        assert!(!result.contains("lines total"));
    }

    // ---- Log 压缩器测试 ----

    #[test]
    fn log_summary_preserves_error_lines() {
        let input = "INFO starting\nINFO processing\nINFO processing\nINFO processing\nINFO processing\nERROR something broke\nINFO cleanup\n";
        let result = format_log_summary("Bash", "call_log1", input);
        assert!(result.contains("Log summarized"));
        assert!(result.contains("ERROR something broke"), "ERROR 行必须保留");
    }

    #[test]
    fn log_summary_preserves_warn_lines() {
        let input = "INFO a\nINFO b\nINFO c\nINFO d\nINFO e\nWARN slow query\nINFO f\n";
        let result = format_log_summary("Bash", "call_log2", input);
        assert!(result.contains("WARN slow query"), "WARN 行必须保留");
    }

    #[test]
    fn log_summary_folds_repeated_patterns() {
        let mut input = String::new();
        for i in 0..20 {
            input.push_str(&format!("Compiling crate_{i} v0.1.0\n"));
        }
        input.push_str("    Finished release [optimized] target(s)\n");
        let result = format_log_summary("Bash", "call_log3", &input);
        assert!(result.contains("similar Compiling"), "重复 Compiling 行应被折叠");
        assert!(result.contains("Finished"), "结果摘要应保留");
    }

    #[test]
    fn log_summary_preserves_tail_lines() {
        let mut input = String::new();
        for i in 0..30 {
            input.push_str(&format!("INFO line {i}\n"));
        }
        input.push_str("test result: ok. 30 passed;\n");
        let result = format_log_summary("Bash", "call_log4", &input);
        assert!(result.contains("test result"), "尾部结果行应保留");
    }

    #[test]
    fn log_summary_includes_stats() {
        let input = "INFO a\nINFO b\nINFO c\nINFO d\nERROR e\nWARN f\n";
        let result = format_log_summary("Bash", "call_log5", input);
        assert!(result.contains("1 critical"), "应统计 critical 数");
        assert!(result.contains("1 warning"), "应统计 warning 数");
        assert!(result.contains("patterns"), "应统计 pattern 数");
    }

    #[test]
    fn log_summary_routes_from_format_summary() {
        let input = "   Compiling proc-macro2 v1.0.81\n   Compiling libc v0.2.153\n    Finished dev [unoptimized] target(s)\n     Running `target/debug/test`\nerror[E0308]: mismatched types\n";
        let result = format_summary("Bash", "call_log6", input);
        assert!(result.contains("Log summarized"), "构建日志应路由到 Log 压缩器");
    }

    // ---- 路由入口测试 ----

    #[test]
    fn format_summary_routes_json() {
        let input = r#"{"key":"value"}"#;
        let result = format_summary("Bash", "call_d", input);
        assert!(result.contains("JSON summarized"));
    }

    #[test]
    fn format_summary_routes_code() {
        let input = "fn foo() {}\n";
        let result = format_summary("Read", "call_e", input);
        assert!(result.contains("Code summarized"));
    }

    #[test]
    fn format_summary_routes_text() {
        let input = "just plain text\nsecond line\n";
        let result = format_summary("Read", "call_f", input);
        assert!(result.contains("Text summarized"));
    }
}
