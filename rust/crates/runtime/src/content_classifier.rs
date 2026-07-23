//! 启发式内容类型检测,用于内容感知路由(microcompact Phase 2)。
//!
//! 设计目标:<1ms 判断,不依赖 ML,前 100 字符 + 必要时整体 JSON 解析已能覆盖 >95% 场景。
//! 与 `content_compression.rs` 配合:`classify()` 决定走哪个压缩器。
//!
//! 详见 `docs/design-headroom-absorption.md` 第 2 节。

use serde_json::Value;

/// 内容类型分类。决定 `format_tool_result_summary` 路由到哪个压缩器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    /// JSON(或以 JSON 为主的文本)。保留结构,压缩叶子值。
    Json,
    /// 源代码。保留签名,折叠实现体。
    Code(CodeLanguage),
    /// 多条目结构化输出(如 Grep 结果、`ls -la` 表格)。保留表头 + 代表性行。
    Tabular,
    /// 普通文本/日志/混合。走原"前 3 行 + 240 chars"逻辑。
    Text,
}

/// 识别出的源代码语言。仅用于 Code 压缩器的签名模式选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLanguage {
    Rust,
    TypeScript,
    Python,
    Go,
    Java,
    /// 识别为代码但语言不确定。Code 压缩器按通用关键字模式处理。
    Unknown,
}

/// 入口:对 tool result 内容做启发式分类。
///
/// 判断顺序(首个命中胜出):
/// 1. JSON — 以 `{` 或 `[` 开头且整体可解析为 JSON
/// 2. Code — 含已知编程语言关键字/模式
/// 3. Tabular — ≥5 行且每行模式相同(分隔符对齐)
/// 4. Text — 兜底
#[must_use]
pub fn classify(content: &str) -> ContentType {
    let trimmed = content.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        // 只有真正能解析为 JSON 才归类为 Json;否则可能是代码块或文本。
        if serde_json::from_str::<Value>(trimmed).is_ok() {
            return ContentType::Json;
        }
    }
    if looks_like_code(content) {
        return ContentType::Code(detect_language(content));
    }
    if is_tabular(content) {
        return ContentType::Tabular;
    }
    ContentType::Text
}

/// 判断内容是否像源代码。
///
/// 启发式:逐行检查(trim 后)是否以已知编程语言关键字**开头**。
/// 用行首匹配而非 `contains`,避免误判英文文本中的 "import statement" 等短语。
/// 只扫描前 20 行,足够覆盖文件头部的 use/import/package 声明。
fn looks_like_code(content: &str) -> bool {
    let patterns: &[&str] = &[
        "//!",
        "///",
        "use ",
        "import ",
        "package ",
        "#include",
        "fn ",
        "def ",
        "func ",
        "function ",
        "class ",
        "struct ",
        "enum ",
        "trait ",
        "impl ",
        "interface ",
        "pub fn",
        "pub async fn",
        "async fn",
        "const ",
        "static ",
        "let ",
        "var ",
        "public class",
        "private class",
    ];
    for line in content.lines().take(20) {
        let trimmed = line.trim_start();
        if patterns.iter().any(|p| trimmed.starts_with(p)) {
            return true;
        }
    }
    false
}

/// 根据关键字模式推断代码语言。
///
/// 逐行扫描,按优先级返回首个命中的语言:Rust > Python > Go > Java > TypeScript。
/// 用行首匹配确保准确性(如 `def ` 只匹配 Python 的函数定义,不匹配英文文本)。
fn detect_language(content: &str) -> CodeLanguage {
    for line in content.lines().take(30) {
        let trimmed = line.trim_start();
        // Rust(优先级最高,关键字最独特)
        if trimmed.starts_with("fn ")
            || trimmed.starts_with("impl ")
            || trimmed.starts_with("trait ")
            || trimmed.starts_with("pub fn")
            || trimmed.starts_with("pub async fn")
            || trimmed.starts_with("async fn")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("use std::")
            || trimmed.starts_with("let mut")
        {
            return CodeLanguage::Rust;
        }
        // Python(强信号:函数定义、lambda、装饰器)
        if trimmed.starts_with("def ")
            || trimmed.starts_with("lambda ")
            || trimmed.starts_with("@dataclass")
            || trimmed.starts_with("from __future__")
            || trimmed.starts_with("elif ")
        {
            return CodeLanguage::Python;
        }
        // Go
        if trimmed.starts_with("package ") || trimmed.starts_with("func ") {
            return CodeLanguage::Go;
        }
        // Java
        if trimmed.starts_with("public class")
            || trimmed.starts_with("private class")
            || trimmed.starts_with("import java.")
            || trimmed.starts_with("System.out")
        {
            return CodeLanguage::Java;
        }
        // TypeScript/JavaScript
        if trimmed.starts_with("function ")
            || trimmed.starts_with("interface ")
            || trimmed.starts_with("export ")
            || trimmed.starts_with("const ")
            || trimmed.contains("=> ")
        {
            return CodeLanguage::TypeScript;
        }
    }
    CodeLanguage::Unknown
}

/// 判断内容是否为表格状(多条目结构化输出)。
///
/// 启发式:
/// - 行数 ≥ 5
/// - 至少 80% 的非空行包含统一的分隔符(空格对齐、`|`、`\t`)
/// - 每行 token 数(按分隔符切分)在 ±1 范围内一致
fn is_tabular(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 5 {
        return false;
    }
    // 选择分隔符:优先 `|`(markdown 表格),其次 `\t`(TSV),最后多个连续空格(对齐列)
    let delimiter = detect_tabular_delimiter(&lines);
    let delimiter = match delimiter {
        Some(d) => d,
        None => return false,
    };
    // 按分隔符切分每行,统计列数;要求 ≥ 80% 行的列数等于众数
    let mut column_counts: Vec<usize> = Vec::with_capacity(lines.len());
    for line in &lines {
        let cols = if delimiter == ' ' {
            // 空格分隔:连续空格视为一个分隔符
            line.split_whitespace().count()
        } else {
            line.split(delimiter).count()
        };
        column_counts.push(cols);
    }
    let mode = most_common(&column_counts).unwrap_or(0);
    if mode < 2 {
        return false; // 单列表格不算 Tabular
    }
    let matching = column_counts.iter().filter(|&&c| c == mode).count();
    let ratio = matching as f64 / column_counts.len() as f64;
    ratio >= 0.8
}

/// 检测表格分隔符:返回 Some('|') / Some('\t') / Some(' ') / None。
fn detect_tabular_delimiter(lines: &[&str]) -> Option<char> {
    // 取前 5 行作为样本
    let sample: Vec<&str> = lines.iter().take(5).copied().collect();
    // 优先级:`|` > `\t` > 多空格
    let pipe_count = sample
        .iter()
        .filter(|l| l.contains('|'))
        .count();
    if pipe_count > sample.len() / 2 {
        return Some('|');
    }
    let tab_count = sample.iter().filter(|l| l.contains('\t')).count();
    if tab_count > sample.len() / 2 {
        return Some('\t');
    }
    // 多空格对齐:≥3 个连续空格,且 ≥ 半数行命中
    let space_count = sample
        .iter()
        .filter(|l| l.contains("   "))
        .count();
    if space_count > sample.len() / 2 {
        return Some(' ');
    }
    None
}

/// 返回 vec 中出现次数最多的元素。
fn most_common<T: PartialEq + Copy>(items: &[T]) -> Option<T> {
    let mut best: Option<(T, usize)> = None;
    for &item in items {
        let count = items.iter().filter(|&&x| x == item).count();
        if best.is_none_or(|(_, c)| count > c) {
            best = Some((item, count));
        }
    }
    best.map(|(item, _)| item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_simple_json_object() {
        let content = r#"{"name":"Alice","age":30}"#;
        assert_eq!(classify(content), ContentType::Json);
    }

    #[test]
    fn classifies_json_array() {
        let content = r#"[1, 2, 3, 4, 5]"#;
        assert_eq!(classify(content), ContentType::Json);
    }

    #[test]
    fn classifies_nested_json() {
        let content = r#"{"users":[{"id":1},{"id":2}]}"#;
        assert_eq!(classify(content), ContentType::Json);
    }

    #[test]
    fn classifies_rust_code() {
        let content = "//! Module doc\nuse std::collections::HashMap;\npub fn foo(x: i32) -> i32 { x + 1 }\n";
        assert_eq!(classify(content), ContentType::Code(CodeLanguage::Rust));
    }

    #[test]
    fn classifies_python_code() {
        let content = "import os\n\ndef main():\n    print('hello')\n";
        assert_eq!(classify(content), ContentType::Code(CodeLanguage::Python));
    }

    #[test]
    fn classifies_typescript_code() {
        let content = "import { foo } from './bar';\nfunction baz(): void {}\n";
        assert_eq!(
            classify(content),
            ContentType::Code(CodeLanguage::TypeScript)
        );
    }

    #[test]
    fn classifies_go_code() {
        let content = "package main\n\nimport \"fmt\"\n\nfunc main() {\n    fmt.Println(\"hi\")\n}\n";
        assert_eq!(classify(content), ContentType::Code(CodeLanguage::Go));
    }

    #[test]
    fn classifies_tabular_with_pipes() {
        let content = "name | age | city\n---- | --- | ----\nAlice | 30 | NYC\nBob | 25 | LA\nCarol | 40 | SF\nDave | 35 | Boston\n";
        assert_eq!(classify(content), ContentType::Tabular);
    }

    #[test]
    fn classifies_tabular_with_spaces() {
        // 5+ 行,每行 4 列,3+ 空格对齐
        let content = "apples   3   red    0.50\nbananas   5   yellow 0.30\ncherries 2   red    0.80\ndates    10  brown  0.60\neggs     12  white  0.20\n";
        assert_eq!(classify(content), ContentType::Tabular);
    }

    #[test]
    fn classifies_plain_text_as_text() {
        let content = "This is a plain text message.\nIt has multiple lines.\nNo code or JSON here.\nJust natural language.\n";
        assert_eq!(classify(content), ContentType::Text);
    }

    #[test]
    fn classifies_log_lines_as_text() {
        let content = "[2026-07-23 10:00:00] INFO Starting server\n[2026-07-23 10:00:01] INFO Listening on :8080\n[2026-07-23 10:00:02] WARN Slow query\n";
        assert_eq!(classify(content), ContentType::Text);
    }

    #[test]
    fn does_not_classify_short_text_as_tabular() {
        // 只有 3 行,不够 Tabular 的 ≥5 行阈值
        let content = "a 1\nb 2\nc 3\n";
        assert_eq!(classify(content), ContentType::Text);
    }

    #[test]
    fn does_not_classify_invalid_json_starting_with_brace() {
        // 以 { 开头但不是合法 JSON — 不应归类为 Json
        let content = "{this is not valid json at all}";
        assert_ne!(classify(content), ContentType::Json);
    }

    #[test]
    fn classifies_rust_with_impl_block() {
        let content = "impl MyStruct {\n    pub fn new() -> Self { Self {} }\n}\n";
        assert_eq!(classify(content), ContentType::Code(CodeLanguage::Rust));
    }

    #[test]
    fn classifies_java_public_class() {
        let content = "public class Main {\n    public static void main(String[] args) {\n        System.out.println(\"hi\");\n    }\n}\n";
        assert_eq!(classify(content), ContentType::Code(CodeLanguage::Java));
    }

    #[test]
    fn empty_content_classifies_as_text() {
        assert_eq!(classify(""), ContentType::Text);
    }

    #[test]
    fn classifies_large_json_api_response() {
        let content = r#"{"users":[{"id":1,"name":"Alice","email":"alice@example.com"},{"id":2,"name":"Bob","email":"bob@example.com"}],"total":2,"page":1}"#;
        assert_eq!(classify(content), ContentType::Json);
    }
}
