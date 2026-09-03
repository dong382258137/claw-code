//! 充分性契约(Sufficiency Contract,P2 · Task 4「给侦察装上价格」)。
//!
//! 问题:模型在取证义务(「事实需要证据」)驱动下可能无限追加只读侦察——
//! 判据永不显式化时,"再多查一次"永远有局部收益。本模块给侦察装上价格:
//! 让模型在发起侦察链前**显式声明停止判据**,框架据此把"何时足够"从
//! 模型的自我感觉变成可校验的机械事实。
//!
//! 协议格式(assistant 文本中的声明标记):
//! ```text
//! [SUFFICIENCY: <源1>, <源2>, ...]
//! ```
//! 其中 `<源>` = 文件路径 / 文档名 / 工具名(如 `.claw/trackers/x.md`、
//! `digest`、`NOTES`)。框架随后登记模型实际读取的源(见
//! [`SufficiencyContract::note_sources_read`]);当声明源全部 ∈ 已读源集合
//! 时判据满足,运行时注入「充分即停」提醒,并把判据满足后仍发起的只读
//! 侦察标记为冗余。未声明判据时全程零干扰。

use std::collections::BTreeSet;

/// 充分性契约声明标记前缀:`[SUFFICIENCY: <源1>, <源2>, ...]`。
pub const SUFFICIENCY_MARKER: &str = "[SUFFICIENCY:";

/// 侦察类工具(不计副作用动作):读文件 / 两类搜索 / 会话检索。
/// bash 是否计入由 [`bash_command_is_read_only`] 单独判定。
pub const RECON_TOOLS: [&str; 4] = ["read_file", "grep_search", "glob_search", "session_search"];

/// 只读 bash 动词(小写)。命令首个 token 命中此集合视为只读侦察。
const READ_ONLY_BASH_VERBS: [&str; 19] = [
    "cat",
    "type",
    "ls",
    "dir",
    "head",
    "tail",
    "wc",
    "more",
    "grep",
    "rg",
    "findstr",
    "find",
    "get-content",
    "get-childitem",
    "select-string",
    "gc",
    "gci",
    "sls",
    "git",
];

/// git 只读子命令(小写)。`git <子命令>` 命中才视为只读。
const GIT_READ_ONLY_SUBCOMMANDS: [&str; 13] = [
    "log",
    "diff",
    "status",
    "show",
    "branch",
    "remote",
    "ls-files",
    "rev-parse",
    "blame",
    "grep",
    "tag",
    "describe",
    "ls-tree",
];

/// 从 assistant 文本提取 `[SUFFICIENCY: ...]` 标记内逗号分隔的源列表。
///
/// - 无标记返回空 Vec;
/// - 同一文本可含多个标记(全部合并);
/// - 空项容错(`,` 间的空白项跳过);
/// - 仅做 trim,引号/大小写/路径前导由 [`normalize_source`] 统一处理。
pub fn parse_sufficiency_declaration(assistant_text: &str) -> Vec<String> {
    let mut sources = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel_start) = assistant_text[cursor..].find(SUFFICIENCY_MARKER) {
        let content_start = cursor + rel_start + SUFFICIENCY_MARKER.len();
        let Some(rel_end) = assistant_text[content_start..].find(']') else {
            // 标记未闭合:放弃(视作无有效声明),避免吞掉后续内容。
            break;
        };
        let content_end = content_start + rel_end;
        // 若标记内容里又出现新的声明前缀,说明当前标记实际未闭合(内容被
        // 后续标记吞并),放弃当前标记、从下一处声明前缀继续扫描。
        if let Some(rel_nested) =
            assistant_text[content_start..content_end].find(SUFFICIENCY_MARKER)
        {
            cursor = content_start + rel_nested;
            continue;
        }
        for part in assistant_text[content_start..content_end].split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                sources.push(trimmed.to_string());
            }
        }
        cursor = content_end + 1;
    }
    sources
}

/// 源归一化:小写、去首尾空白与引号、`\`→`/`、去文件路径前导 `./`/`../`。
///
/// 纯工具名/文档名(如 `digest`、`NOTES`)原样保留(仅小写)。声明源与已读
/// 源各自归一化后做集合包含判定,路径写法差异(引号/大小写/前导符)不
/// 影响匹配。
pub fn normalize_source(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // 去首尾引号(单/双),再去一次空白(引号剥离后可能露出空白)。
    s = s.trim_matches('"').trim_matches('\'').trim().to_string();
    // Windows 反斜杠路径统一为正斜杠,便于跨平台匹配。
    s = s.replace('\\', "/");
    // 去文件路径前导 `./` 与 `../`(可叠加)。
    loop {
        if let Some(rest) = s.strip_prefix("./") {
            s = rest.to_string();
        } else if let Some(rest) = s.strip_prefix("../") {
            s = rest.to_string();
        } else {
            break;
        }
    }
    s.to_lowercase()
}

/// 会话级充分性契约(挂在 [`crate::conversation::ConversationRuntime`] 上)。
///
/// 生命周期:turn 开头 [`reset`](SufficiencyContract::reset);assistant 文本
/// 出现声明标记时 [`declare`](SufficiencyContract::declare)(替换旧判据);
/// 侦察类工具执行中登记已读源;声明源全部已读 → `satisfied == true`。
#[derive(Debug, Default)]
pub struct SufficiencyContract {
    /// 已声明的停止判据源(已归一化、去重、保序)。
    declared_sources: Vec<String>,
    /// 已读取源集合(归一化后)。
    read_sources: BTreeSet<String>,
    /// 判据是否已满足(声明源 ⊆ 已读源;未声明恒 false)。
    satisfied: bool,
}

impl SufficiencyContract {
    /// (重新)声明判据:归一化 + 去重后替换声明源,并清空已读状态。
    /// 传入空 Vec 等价于复位(未声明,恒不满足)。
    pub fn declare(&mut self, raw_sources: Vec<String>) {
        self.declared_sources.clear();
        let mut seen = BTreeSet::new();
        for raw in raw_sources {
            let normalized = normalize_source(&raw);
            if !normalized.is_empty() && seen.insert(normalized.clone()) {
                self.declared_sources.push(normalized);
            }
        }
        self.read_sources.clear();
        self.recompute_satisfied();
    }

    /// 复位到未声明状态(turn 开头调用,上一轮契约不跨 turn 延续)。
    pub fn reset(&mut self) {
        self.declared_sources.clear();
        self.read_sources.clear();
        self.satisfied = false;
    }

    /// 是否已声明判据(声明源非空)。
    #[must_use]
    pub fn is_declared(&self) -> bool {
        !self.declared_sources.is_empty()
    }

    /// 登记一次读取涉及的源(已归一化后并入已读集合),并重算判据满足态。
    /// 未声明判据时零记账(无谓增长)。
    pub fn note_sources_read<I>(&mut self, sources: I)
    where
        I: IntoIterator<Item = String>,
    {
        if self.declared_sources.is_empty() {
            return;
        }
        for source in sources {
            let normalized = normalize_source(&source);
            if !normalized.is_empty() {
                self.read_sources.insert(normalized);
            }
        }
        self.recompute_satisfied();
    }

    /// 判据是否已满足:声明源全部 ∈ 已读源集合。未声明恒 false(空集
    /// 的"全称真"在这里是无意义的,必须显式守卫)。
    #[must_use]
    pub fn is_contract_satisfied(&self) -> bool {
        self.satisfied
    }

    fn recompute_satisfied(&mut self) {
        self.satisfied = !self.declared_sources.is_empty()
            && self
                .declared_sources
                .iter()
                .all(|source| self.read_sources.contains(source));
    }
}

/// 判断一次工具调用是否为"侦察类"(只读查询/读取)。
///
/// - read_file / grep_search / glob_search / session_search:恒为侦察;
/// - bash:仅当命令命中只读动词集合(粗粒度)才算。
#[must_use]
pub fn is_recon_tool(tool_name: &str, input: &str) -> bool {
    if RECON_TOOLS.contains(&tool_name) {
        return true;
    }
    tool_name == "bash" && bash_command_is_read_only(input)
}

/// bash 命令是否只读(粗粒度:首个 token 命中只读动词集合;git 额外校验
/// 子命令)。非只读(构建/写操作/任意脚本)不计入侦察,避免把正常执行
/// 误标为冗余。
#[must_use]
pub fn bash_command_is_read_only(input: &str) -> bool {
    let command = bash_command_text(input);
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return false;
    };
    let verb = first.to_lowercase();
    if verb == "git" {
        return tokens
            .get(1)
            .map(|sub| {
                let lower = sub.to_lowercase();
                GIT_READ_ONLY_SUBCOMMANDS.contains(&lower.as_str())
            })
            .unwrap_or(false);
    }
    READ_ONLY_BASH_VERBS.contains(&verb.as_str())
}

/// 提取一次侦察类工具调用"涉及"的候选源(粗粒度;统一小写去引号等由
/// 调用方经 [`normalize_source`] 完成)。bash 从命令文本里粗略提取路径
/// token;非侦察工具返回空。
#[must_use]
pub fn recon_sources_from_tool(tool_name: &str, input: &str) -> Vec<String> {
    match tool_name {
        "read_file" => json_candidates(input, &["file_path", "path"], true),
        "grep_search" => json_candidates(input, &["pattern", "path", "glob"], true),
        "glob_search" => json_candidates(input, &["pattern", "path", "glob"], true),
        "session_search" => json_candidates(input, &["query"], true),
        "bash" => {
            if bash_command_is_read_only(input) {
                bash_path_tokens(input)
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// 从工具 input 中按 JSON 字段提取候选源(全值保留,不拆分单词)。
/// JSON 解析失败且 `fallback_raw == true` 时把整个 input 当作单个候选
/// (测试/简写路径经常直接传裸路径字符串)。
fn json_candidates(input: &str, fields: &[&str], fallback_raw: bool) -> Vec<String> {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::Object(map)) => fields
            .iter()
            .filter_map(|field| map.get(*field).and_then(|v| v.as_str()))
            .map(|value| value.to_string())
            .collect(),
        Ok(_) => Vec::new(),
        Err(_) if fallback_raw => {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }
        }
        Err(_) => Vec::new(),
    }
}

/// 取 bash 命令文本:input 为 JSON 时读 `command` 字段;否则整段视为命令。
fn bash_command_text(input: &str) -> String {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(input) {
        for field in ["command", "cmd"] {
            if let Some(value) = map.get(field).and_then(|v| v.as_str()) {
                return value.to_string();
            }
        }
    }
    input.to_string()
}

/// 从 bash 命令文本粗略提取路径 token(粗粒度):去引号、跳过动词与
/// `-` 开头 flag,保留含路径特征(`/`、`\`、`.`)的 token。
fn bash_path_tokens(input: &str) -> Vec<String> {
    let tokens: Vec<String> = bash_command_text(input)
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string()
        })
        .filter(|token| !token.is_empty())
        .collect();
    let is_git = tokens
        .first()
        .is_some_and(|t| t.eq_ignore_ascii_case("git"));
    // 跳过动词:git 需再跳一个子命令,其余跳首个 token。
    let skip = if is_git { 2 } else { 1 };
    tokens
        .into_iter()
        .skip(skip)
        .filter(|token| !token.starts_with('-'))
        .filter(|token| token.contains('/') || token.contains('\\') || token.contains('.'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// parse:含标记文本提取多个源(含带前导符的路径与纯工具名)。
    #[test]
    fn parse_extracts_multiple_sources_from_marker() {
        let text = "先核对两个来源再作答。[SUFFICIENCY: .claw/trackers/x.md, digest] 然后给结论。";
        assert_eq!(
            parse_sufficiency_declaration(text),
            vec![".claw/trackers/x.md", "digest"]
        );
    }

    /// parse:无标记返回空。
    #[test]
    fn parse_returns_empty_without_marker() {
        assert!(parse_sufficiency_declaration("直接回答即可,无需查证。").is_empty());
        assert!(parse_sufficiency_declaration("").is_empty());
    }

    /// parse:空项容错 + 引号/大小写原样保留(归一化在 normalize 阶段)。
    #[test]
    fn parse_tolerates_empty_items_and_keeps_quotes() {
        let text = "[SUFFICIENCY: a.md, , \"B.md\", 'c.md',   ]";
        assert_eq!(
            parse_sufficiency_declaration(text),
            vec!["a.md", "\"B.md\"", "'c.md'"]
        );
    }

    /// parse:同段多个标记全部合并;未闭合标记放弃(不 panic)。
    #[test]
    fn parse_merges_multiple_markers_and_tolerates_unclosed() {
        let text = "[SUFFICIENCY: a.md][SUFFICIENCY: b.md, c.md]";
        assert_eq!(
            parse_sufficiency_declaration(text),
            vec!["a.md", "b.md", "c.md"]
        );
        // 未闭合:找不到 ']' → 放弃该标记及其后内容。
        assert!(parse_sufficiency_declaration("[SUFFICIENCY: a.md").is_empty());
        // 未闭合在前、闭合在后:前一个放弃,后一个仍生效。
        let mixed = "[SUFFICIENCY: a.md\n[SUFFICIENCY: b.md]";
        assert_eq!(parse_sufficiency_declaration(mixed), vec!["b.md"]);
    }

    /// normalize:小写 + 去空白/引号 + `\`→`/` + 去 `./`/`../` 前导。
    #[test]
    fn normalize_lowercases_trims_quotes_and_strips_prefix() {
        assert_eq!(normalize_source("  ./A.md  "), "a.md");
        assert_eq!(normalize_source("..\\SRC\\Main.rs"), "src/main.rs");
        assert_eq!(normalize_source("\"B.md\""), "b.md");
        assert_eq!(normalize_source("'./C.md'"), "c.md");
        assert_eq!(
            normalize_source(".claw/trackers/x.md"),
            ".claw/trackers/x.md"
        );
        assert_eq!(normalize_source("DIGEST"), "digest");
        assert_eq!(normalize_source("NOTES"), "notes");
    }

    /// 满足判定:声明 2 源 → 读 1 个 false → 读第 2 个 true。
    #[test]
    fn contract_satisfied_only_after_all_declared_sources_read() {
        let mut contract = SufficiencyContract::default();
        contract.declare(vec!["a.md".to_string(), "./B.md".to_string()]);
        assert!(!contract.is_contract_satisfied());
        contract.note_sources_read(vec!["A.md".to_string()]);
        assert!(!contract.is_contract_satisfied(), "只读一个不应满足");
        contract.note_sources_read(vec!["b.md".to_string()]);
        assert!(
            contract.is_contract_satisfied(),
            "声明源全部读取后应满足(含大小写/前导符归一化)"
        );
    }

    /// 满足判定:声明 0 源恒 false(空集全称真的守卫)。
    #[test]
    fn contract_undeclared_never_satisfied() {
        let mut contract = SufficiencyContract::default();
        contract.note_sources_read(vec!["a.md".to_string(), "b.md".to_string()]);
        assert!(!contract.is_declared());
        assert!(!contract.is_contract_satisfied());
        // 显式空声明同样恒 false。
        contract.declare(Vec::new());
        contract.note_sources_read(vec!["x.md".to_string()]);
        assert!(!contract.is_contract_satisfied());
    }

    /// declare 覆盖旧判据:新声明替换旧声明并清空已读状态。
    #[test]
    fn declare_replaces_previous_contract() {
        let mut contract = SufficiencyContract::default();
        contract.declare(vec!["a.md".to_string()]);
        contract.note_sources_read(vec!["a.md".to_string()]);
        assert!(contract.is_contract_satisfied());
        contract.declare(vec!["b.md".to_string()]);
        assert!(!contract.is_contract_satisfied(), "新判据需要重新读取");
        contract.note_sources_read(vec!["b.md".to_string()]);
        assert!(contract.is_contract_satisfied());
    }

    /// 工具源提取:read_file(JSON 字段 + 裸路径回退)、grep/glob(多字段)、
    /// session_search(query)。
    #[test]
    fn recon_sources_extracted_per_tool() {
        assert_eq!(
            recon_sources_from_tool("read_file", r#"{"file_path":"a.md"}"#),
            vec!["a.md"]
        );
        // 裸路径回退(非 JSON)。
        assert_eq!(recon_sources_from_tool("read_file", "a.md"), vec!["a.md"]);
        // grep:pattern + path 都作为候选源。
        let mut got =
            recon_sources_from_tool("grep_search", r#"{"pattern":"sufficiency","path":"src"}"#);
        got.sort();
        assert_eq!(got, vec!["src", "sufficiency"]);
        assert_eq!(
            recon_sources_from_tool("session_search", r#"{"query":"digest"}"#),
            vec!["digest"]
        );
        // 非侦察工具 → 空。
        assert!(recon_sources_from_tool("write_file", r#"{"file_path":"a.md"}"#).is_empty());
    }

    /// bash:只读命令识别 + 路径 token 提取;写操作/构建不算侦察。
    #[test]
    fn bash_read_only_detection_and_path_tokens() {
        assert!(bash_command_is_read_only(r#"{"command":"cat a.md"}"#));
        assert!(bash_command_is_read_only(
            r#"{"command":"Get-Content .\\a.md"}"#
        ));
        assert!(bash_command_is_read_only(r#"{"command":"git log --stat"}"#));
        assert!(!bash_command_is_read_only(
            r#"{"command":"git commit -m x"}"#
        ));
        assert!(!bash_command_is_read_only(r#"{"command":"cargo build"}"#));
        assert!(!bash_command_is_read_only(
            r#"{"command":"python setup.py"}"#
        ));

        // git log 的路径 token 提取(跳过 git + 子命令 + flag)。
        let got = recon_sources_from_tool("bash", r#"{"command":"git log --stat ./src/main.rs"}"#);
        assert_eq!(got, vec!["./src/main.rs"]);

        // cat 只读命令的路径 token(跳过动词 cat)。
        let got = recon_sources_from_tool("bash", r#"{"command":"cat README.md"}"#);
        assert_eq!(got, vec!["README.md"]);

        // 非只读 bash(构建)不产出候选源。
        assert!(recon_sources_from_tool("bash", r#"{"command":"cargo build"}"#).is_empty());
    }
}
