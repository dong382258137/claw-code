//! 受控记忆词表（subject / attribute 枚举）。
//!
//! 背景：提取器自由生成 subject 会导致同一事实在长程对话中漂移出多个
//! subject（`主数据库` / `项目技术栈` / `project` …），upsert 判重永远命中
//! 不到旧条目，最终并存多条互相矛盾的事实。本模块把 subject 收敛为**固定
//! 枚举**，attribute 收敛为每个 subject 下的固定属性名，并在写入前做归一化。
//!
//! 词表可由 `<workspace>/.claw/memory_taxonomy.json` 覆盖（追加类目不需要改
//! 代码），也可用环境变量 `CLAW_MEMORY_TAXONOMY` 指向自定义路径：
//!
//! ```json
//! {
//!   "subjects": [
//!     {"name": "cache", "attributes": ["engine", "ttl"], "aliases": ["缓存", "redis"]}
//!   ],
//!   "attribute_aliases": [
//!     {"subject": "cache", "alias": "过期时间", "canonical": "ttl"}
//!   ]
//! }
//! ```
//!
//! 配置与内置默认词表**合并**：同名 subject 覆盖其属性/别名，新 subject 追加。
//! 未命中词表的 subject 一律归入 `other`，绝不允许提取器自造类目。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::memory_extract::MemoryOp;

/// 词表配置文件名（与 `memory.json` 同目录）。
pub const TAXONOMY_FILE: &str = "memory_taxonomy.json";

/// 无法归类时的兜底 subject。
pub const OTHER_SUBJECT: &str = "other";

/// 一个受控 subject 及其允许的 attribute。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectSpec {
    pub name: String,
    #[serde(default)]
    pub attributes: Vec<String>,
    /// 自由文本别名 / 关键词：用于把旧数据与模型输出映射到该 subject。
    #[serde(default)]
    pub aliases: Vec<String>,
    /// 主事实判定词：`content` 命中时，attribute 一律收口到第一个属性。
    ///
    /// 用于抵消模型把同一事实拆成 `engine` / `version` / `port` 的漂移
    /// （例如「主数据库使用 MySQL」无论被标成什么属性都归 `primary`）。
    #[serde(default)]
    pub primary_keywords: Vec<String>,
}

/// attribute 同义词：`alias`（可限定 `subject`）→ 规范 `canonical`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributeAlias {
    /// `None` 表示对所有 subject 生效。
    #[serde(default)]
    pub subject: Option<String>,
    pub alias: String,
    pub canonical: String,
}

/// 受控词表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryTaxonomy {
    subjects: Vec<SubjectSpec>,
    attribute_aliases: Vec<AttributeAlias>,
}

/// 外部配置的解析形态（与内部结构一致，便于直接反序列化）。
#[derive(Debug, Clone, Default, Deserialize)]
struct TaxonomyConfig {
    #[serde(default)]
    subjects: Vec<SubjectSpec>,
    #[serde(default)]
    attribute_aliases: Vec<AttributeAlias>,
}

impl Default for MemoryTaxonomy {
    fn default() -> Self {
        Self {
            subjects: default_subjects(),
            attribute_aliases: default_attribute_aliases(),
        }
    }
}

fn spec(name: &str, attributes: &[&str], aliases: &[&str]) -> SubjectSpec {
    SubjectSpec {
        name: name.to_string(),
        attributes: attributes.iter().map(|s| (*s).to_string()).collect(),
        aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
        primary_keywords: Vec::new(),
    }
}

fn alias(subject: &str, alias: &str, canonical: &str) -> AttributeAlias {
    AttributeAlias {
        subject: Some(subject.to_string()),
        alias: alias.to_string(),
        canonical: canonical.to_string(),
    }
}

fn default_subjects() -> Vec<SubjectSpec> {
    let mut subjects = vec![
        spec(
            "database",
            &[
                "primary",
                "port",
                "host",
                "version",
                "engine",
                "credentials",
            ],
            &[
                "数据库",
                "数据存储",
                "存储引擎",
                "主库",
                "主数据库",
                "项目数据库",
                "项目技术栈",
                "db",
                "database",
                "mysql",
                "postgres",
                "postgresql",
                "sql",
                "sqlite",
                "redis",
                "mongodb",
            ],
        ),
        spec(
            "editor",
            &["name", "indent_style", "keybinding", "theme"],
            &[
                "编辑器",
                "编辑器偏好",
                "ide",
                "vscode",
                "neovim",
                "vim",
                "emacs",
                "jetbrains",
                "editor",
            ],
        ),
        spec(
            "runtime",
            &["name", "version"],
            &[
                "语言运行时",
                "运行时",
                "runtime",
                "node",
                "nodejs",
                "python",
                "rust",
                "jdk",
                "java",
                "golang",
                "语言版本",
            ],
        ),
        spec(
            "build",
            &["name", "version", "registry"],
            &[
                "构建",
                "构建工具",
                "包管理",
                "包管理器",
                "依赖管理",
                "工具链",
                "build",
                "package manager",
                "cargo",
                "npm",
                "pnpm",
                "yarn",
                "pip",
                "poetry",
                "gradle",
                "maven",
            ],
        ),
        spec(
            "test",
            &["framework", "command", "runner"],
            &[
                "测试",
                "测试框架",
                "单元测试",
                "test",
                "testing",
                "pytest",
                "jest",
                "vitest",
                "junit",
            ],
        ),
        spec(
            "ui",
            &["theme", "font", "density", "layout"],
            &[
                "主题", "界面", "外观", "配色", "字体", "深色", "浅色", "ui", "theme",
            ],
        ),
        spec(
            "language_pref",
            &["reply_language", "comment_language", "code_language"],
            &[
                "语言偏好",
                "回复语言",
                "回答语言",
                "输出语言",
                "language preference",
                "中文",
                "英文",
            ],
        ),
        spec(
            "work_style",
            &[
                "rule",
                "preference",
                "process",
                // 风格观察通道（`extract_style_signals`）专用的细粒度属性。
                // 必须**互不相同**：`apply_upsert` 按 subject+attribute 覆盖旧条目，
                // 若「简洁」「结构化输出」「第一性原理」共用一个 attribute，
                // 后写入的会逐条顶掉先写入的，最终只剩最后一条。
                "answer_length",
                "output_format",
                "autonomy",
                "reasoning_style",
                "sourcing",
            ],
            &[
                "工作方式",
                "工作风格",
                "工作习惯",
                "协作方式",
                "流程约束",
                "工作约束",
                "work flow",
                "workflow",
                "work style",
            ],
        ),
        spec(
            "project",
            &["vendor_dir", "path", "policy", "dependency", "convention"],
            &["项目", "项目级", "目录", "仓库", "project"],
        ),
        spec(
            "profile",
            &["role", "name", "locale", "timezone"],
            &["用户", "用户画像", "身份", "个人", "profile", "user"],
        ),
        spec(OTHER_SUBJECT, &["other"], &[]),
    ];
    if let Some(database) = subjects.iter_mut().find(|spec| spec.name == "database") {
        database.primary_keywords = [
            "主数据库",
            "主库",
            "primary database",
            "main database",
            "primary db",
        ]
        .iter()
        .map(|keyword| (*keyword).to_string())
        .collect();
    }
    subjects
}

fn default_attribute_aliases() -> Vec<AttributeAlias> {
    vec![
        alias("database", "主数据库", "primary"),
        alias("database", "数据库选择", "primary"),
        alias("database", "用哪个", "primary"),
        alias("database", "技术栈选了", "primary"),
        alias("database", "技术栈", "primary"),
        alias("database", "默认", "primary"),
        alias("database", "main", "primary"),
        alias("database", "default", "primary"),
        alias("database", "端口", "port"),
        alias("database", "版本", "version"),
        alias("database", "引擎", "engine"),
        alias("database", "类型", "engine"),
        alias("database", "主机", "host"),
        alias("database", "地址", "host"),
        alias("database", "凭证", "credentials"),
        alias("editor", "缩进", "indent_style"),
        alias("editor", "缩进风格", "indent_style"),
        alias("editor", "indent", "indent_style"),
        alias("editor", "快捷键", "keybinding"),
        alias("editor", "主题", "theme"),
        alias("ui", "主题", "theme"),
        alias("ui", "配色", "theme"),
        alias("ui", "字体", "font"),
        alias("runtime", "语言", "name"),
        alias("runtime", "版本", "version"),
        alias("build", "包管理器", "name"),
        alias("build", "构建工具", "name"),
        alias("test", "框架", "framework"),
        alias("language_pref", "语言", "reply_language"),
        alias("work_style", "约束", "rule"),
        alias("work_style", "要求", "rule"),
    ]
}

impl MemoryTaxonomy {
    /// 从 `<state_dir>/memory_taxonomy.json` 加载（缺失/损坏时用默认词表）。
    ///
    /// `CLAW_MEMORY_TAXONOMY` 可覆盖文件路径。配置是**追加式**的：同名
    /// subject 覆盖属性/别名，新 subject 追加，`other` 始终保留。
    #[must_use]
    pub fn load(state_dir: &Path) -> Self {
        let path = std::env::var("CLAW_MEMORY_TAXONOMY")
            .ok()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| state_dir.join(TAXONOMY_FILE));
        let config = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<TaxonomyConfig>(&raw).ok());
        match config {
            Some(config) => Self::default().merged_with(config),
            None => Self::default(),
        }
    }

    /// 合并外部配置（同名覆盖，新名追加）。
    fn merged_with(mut self, config: TaxonomyConfig) -> Self {
        for incoming in config.subjects {
            let name = incoming.name.trim().to_string();
            if name.is_empty() {
                continue;
            }
            match self.subjects.iter_mut().find(|s| s.name == name) {
                Some(existing) => {
                    if !incoming.attributes.is_empty() {
                        existing.attributes = incoming.attributes;
                    }
                    if !incoming.aliases.is_empty() {
                        existing.aliases = incoming.aliases;
                    }
                }
                None => self.subjects.push(SubjectSpec { name, ..incoming }),
            }
        }
        for incoming in config.attribute_aliases {
            if !incoming.alias.trim().is_empty() && !incoming.canonical.trim().is_empty() {
                self.attribute_aliases.push(incoming);
            }
        }
        self.ensure_other();
        self
    }

    fn ensure_other(&mut self) {
        if !self.subjects.iter().any(|s| s.name == OTHER_SUBJECT) {
            self.subjects.push(spec(OTHER_SUBJECT, &["other"], &[]));
        }
    }

    /// 所有受控 subject 名（用于诊断 / 报告）。
    #[must_use]
    pub fn subject_names(&self) -> Vec<&str> {
        self.subjects.iter().map(|s| s.name.as_str()).collect()
    }

    /// 某个 subject 允许的属性名（`other` 返回空表示允许 slug）。
    #[must_use]
    pub fn attributes_for(&self, subject: &str) -> &[String] {
        self.subjects
            .iter()
            .find(|s| s.name == subject)
            .map(|s| s.attributes.as_slice())
            .unwrap_or(&[])
    }

    /// 归一化一条 op 的 subject / attribute（原地修改）。
    ///
    /// 只在 subject 存在时生效：没有 subject 的 op（工具路径）保持不变。
    pub fn normalize_op(&self, op: &mut MemoryOp) {
        let Some(raw) = op.subject.as_deref() else {
            return;
        };
        let subject = self.normalize_subject(Some(raw), op.attribute.as_deref());
        let attribute = self.normalize_attribute(&subject, op.attribute.as_deref());
        op.attribute = Some(self.refine_attribute(&subject, attribute, &op.content));
        op.subject = Some(subject);
    }

    /// 内容感知的属性收口：`content` 命中主事实关键词时回到默认属性。
    ///
    /// 模型偶尔把「主数据库使用 MySQL」标成 `engine` 或 `version`，导致同一
    /// 事实在新属性下另起一条。这里用确定性的关键词规则把它拉回 `primary`。
    fn refine_attribute(&self, subject: &str, attribute: Option<String>, content: &str) -> String {
        let Some(spec) = self.subjects.iter().find(|spec| spec.name == subject) else {
            return attribute.unwrap_or_else(|| OTHER_SUBJECT.to_string());
        };
        let Some(default) = spec.attributes.first() else {
            return attribute.unwrap_or_else(|| OTHER_SUBJECT.to_string());
        };
        let normalized_content = normalize_token(content);
        let is_primary_fact = spec
            .primary_keywords
            .iter()
            .any(|keyword| contains_keyword(&normalized_content, &normalize_token(keyword)));
        if is_primary_fact {
            default.clone()
        } else {
            attribute.unwrap_or_else(|| OTHER_SUBJECT.to_string())
        }
    }

    /// 把自由文本 subject 解析为规范 subject；无法解析返回 `None`。
    ///
    /// 解析顺序：规范名/别名精确命中 → attribute 指名的类目 → 关键词包含
    /// 匹配（取最长）。`project` / `profile` / `other` 视为泛化类目：若
    /// attribute 明确指向某个具体类目（如 `主数据库`），优先具体类目。
    #[must_use]
    pub fn resolve_subject(&self, raw: Option<&str>, attribute: Option<&str>) -> Option<String> {
        let raw = raw?.trim();
        if raw.is_empty() {
            return None;
        }
        let key = normalize_token(raw);
        if key.is_empty() {
            return None;
        }
        let attr_key = attribute
            .map(normalize_token)
            .filter(|value| !value.is_empty());
        let attr_match = attr_key.as_deref().and_then(|value| {
            self.match_exact(value)
                .or_else(|| self.match_contains(value))
        });
        if let Some(name) = self.match_exact(&key) {
            if !is_generic(&name) {
                return Some(name);
            }
            if let Some(attr_name) = attr_match {
                if !is_generic(&attr_name) {
                    return Some(attr_name);
                }
            }
            return Some(name);
        }
        if let Some(name) = attr_match {
            return Some(name);
        }
        self.match_contains(&key)
    }

    /// 规范 subject；无法解析时归 `other`（绝不返回自由文本）。
    #[must_use]
    pub fn normalize_subject(&self, raw: Option<&str>, attribute: Option<&str>) -> String {
        self.resolve_subject(raw, attribute)
            .unwrap_or_else(|| OTHER_SUBJECT.to_string())
    }

    /// 规范 attribute：subject 内规范名 → 同义词 → `other` / slug。
    ///
    /// 已知 subject 下未命中的属性归 `other`；`other` subject 下保留
    /// 确定性 slug（不同事实不会因为都叫 `other` 而互相覆盖）。
    #[must_use]
    pub fn normalize_attribute(&self, subject: &str, raw: Option<&str>) -> Option<String> {
        let raw = raw?.trim();
        if raw.is_empty() {
            return Some(OTHER_SUBJECT.to_string());
        }
        let key = normalize_token(raw);
        let spec = self.subjects.iter().find(|s| s.name == subject);
        if let Some(spec) = spec {
            if let Some(attr) = spec
                .attributes
                .iter()
                .find(|attr| normalize_token(attr) == key)
            {
                return Some(attr.clone());
            }
        }
        if let Some(canonical) = self.lookup_attribute_alias(subject, &key) {
            return Some(canonical);
        }
        if subject == OTHER_SUBJECT {
            return Some(slugify(raw));
        }
        if let Some(spec) = spec {
            if let Some(attr) = spec
                .attributes
                .iter()
                .find(|attr| contains_keyword(&key, &normalize_token(attr)))
            {
                return Some(attr.clone());
            }
        }
        Some(OTHER_SUBJECT.to_string())
    }

    fn lookup_attribute_alias(&self, subject: &str, key: &str) -> Option<String> {
        let scoped = |candidate: &AttributeAlias| {
            candidate
                .subject
                .as_deref()
                .is_none_or(|scope| scope == subject)
        };
        // 精确同义词优先。
        if let Some(hit) = self
            .attribute_aliases
            .iter()
            .filter(|candidate| scoped(candidate))
            .find(|candidate| normalize_token(&candidate.alias) == key)
        {
            return Some(hit.canonical.clone());
        }
        // 再退到包含匹配（如「技术栈选了」包含「技术栈」），取最长别名。
        self.attribute_aliases
            .iter()
            .filter(|candidate| scoped(candidate))
            .filter_map(|candidate| {
                let alias = normalize_token(&candidate.alias);
                contains_keyword(key, &alias).then(|| (alias.chars().count(), candidate))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, candidate)| candidate.canonical.clone())
    }

    fn match_exact(&self, key: &str) -> Option<String> {
        self.subjects
            .iter()
            .find(|spec| {
                normalize_token(&spec.name) == key
                    || spec
                        .aliases
                        .iter()
                        .any(|alias| normalize_token(alias) == key)
            })
            .map(|spec| spec.name.clone())
    }

    fn match_contains(&self, key: &str) -> Option<String> {
        let mut best: Option<(usize, bool, String)> = None;
        for spec in &self.subjects {
            let mut candidates: Vec<&str> = Vec::with_capacity(spec.aliases.len() + 1);
            candidates.push(spec.name.as_str());
            candidates.extend(spec.aliases.iter().map(String::as_str));
            for candidate in candidates {
                let alias = normalize_token(candidate);
                if alias.chars().count() < 2 || !contains_keyword(key, &alias) {
                    continue;
                }
                let score = alias.chars().count();
                let generic = is_generic(&spec.name);
                let better = match &best {
                    None => true,
                    Some((best_len, best_generic, _)) => {
                        score > *best_len || (score == *best_len && *best_generic && !generic)
                    }
                };
                if better {
                    best = Some((score, generic, spec.name.clone()));
                }
            }
        }
        best.map(|(_, _, name)| name)
    }

    /// 注入提取器 prompt 的词表说明。
    #[must_use]
    pub fn prompt_block(&self) -> String {
        let mut out = String::from("subject 只能取下面这些枚举值之一，**不得自造新 subject**：\n");
        for spec in &self.subjects {
            let attrs = if spec.name == OTHER_SUBJECT {
                "other".to_string()
            } else {
                spec.attributes.join(" / ")
            };
            out.push_str(&format!("- {}：attribute 取 {}\n", spec.name, attrs));
        }
        out.push_str(
            "attribute 必须取所属 subject 列出的属性名；都匹配不上时 subject=other、\
             attribute=other。\n\
             同一事实在不同轮次必须使用完全相同的 subject 与 attribute；值发生变化\
             （例如换数据库）时也复用同一 subject+attribute，直接给新内容，不要另造 subject。\n",
        );
        out
    }
}

fn is_generic(name: &str) -> bool {
    matches!(name, "project" | "profile" | OTHER_SUBJECT)
}

/// 归一化 token：小写 + 去空白/常见标点，用于别名比较。
fn normalize_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(
                    c,
                    '-' | '_'
                        | '/'
                        | '\\'
                        | '.'
                        | ':'
                        | '：'
                        | '、'
                        | ','
                        | '，'
                        | '。'
                        | ';'
                        | '；'
                        | '"'
                        | '\''
                        | '`'
                        | '('
                        | ')'
                        | '（'
                        | '）'
                        | '['
                        | ']'
                        | '【'
                        | '】'
                        | '？'
                        | '?'
                        | '!'
                        | '！'
                )
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// 关键词包含判定：ASCII 关键词要求词边界，避免 `ide` 命中 `video`。
fn contains_keyword(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || haystack.is_empty() {
        return false;
    }
    if !needle.chars().all(|c| c.is_ascii_alphanumeric()) {
        return haystack.contains(needle);
    }
    let bytes = haystack.as_bytes();
    let mut search_from = 0;
    while let Some(offset) = haystack[search_from..].find(needle) {
        let start = search_from + offset;
        let end = start + needle.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        search_from = end;
    }
    false
}

/// 确定性 slug：未知 attribute 的兜底（同一文本永远得到同一结果）。
fn slugify(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_sep = false;
    for ch in raw.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(ch);
        } else if !out.is_empty() {
            pending_sep = true;
        }
    }
    if out.is_empty() {
        OTHER_SUBJECT.to_string()
    } else {
        out
    }
}

/// 把一组自由文本 subject 映射到词表；返回 `(规范 subject, 是否孤儿)`。
///
/// 迁移旧数据时使用：能映射则映射，不能映射则标孤儿，不静默保留自由文本。
#[must_use]
pub fn classify_subject(
    taxonomy: &MemoryTaxonomy,
    raw: Option<&str>,
    attribute: Option<&str>,
) -> (Option<String>, bool) {
    match raw {
        None => (None, false),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return (None, false);
            }
            match taxonomy.resolve_subject(Some(trimmed), attribute) {
                Some(subject) => {
                    let known = taxonomy.subjects.iter().any(|spec| spec.name == subject);
                    (Some(subject), !known)
                }
                None => (Some(OTHER_SUBJECT.to_string()), true),
            }
        }
    }
}

/// 已注册但未使用的 subject 集合（供测试断言默认词表不含重复）。
#[must_use]
pub fn duplicate_subjects(taxonomy: &MemoryTaxonomy) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut dups = Vec::new();
    for subject in &taxonomy.subjects {
        if !seen.insert(subject.name.clone()) {
            dups.push(subject.name.clone());
        }
    }
    dups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_taxonomy_has_no_duplicate_subjects() {
        let taxonomy = MemoryTaxonomy::default();
        assert!(duplicate_subjects(&taxonomy).is_empty());
        assert!(taxonomy.subject_names().contains(&"database"));
        assert!(taxonomy.subject_names().contains(&OTHER_SUBJECT));
    }

    #[test]
    fn free_text_database_subjects_collapse() {
        let taxonomy = MemoryTaxonomy::default();
        for raw in [
            "主数据库",
            "项目数据库",
            "项目技术栈",
            "数据库",
            "db",
            "MySQL",
        ] {
            assert_eq!(
                taxonomy.normalize_subject(Some(raw), None),
                "database",
                "{raw} should map to database"
            );
        }
        // 泛化 subject + 明确 attribute 时优先具体类目。
        assert_eq!(
            taxonomy.normalize_subject(Some("project"), Some("主数据库")),
            "database"
        );
    }

    #[test]
    fn unknown_subject_falls_back_to_other() {
        let taxonomy = MemoryTaxonomy::default();
        assert_eq!(
            taxonomy.normalize_subject(Some("某个没见过的类目"), None),
            OTHER_SUBJECT
        );
    }

    #[test]
    fn database_attributes_converge_to_primary() {
        let taxonomy = MemoryTaxonomy::default();
        for raw in ["主数据库", "用哪个", "技术栈选了", "primary", "default"] {
            assert_eq!(
                taxonomy
                    .normalize_attribute("database", Some(raw))
                    .as_deref(),
                Some("primary"),
                "{raw} should map to primary"
            );
        }
        assert_eq!(
            taxonomy
                .normalize_attribute("database", Some("端口"))
                .as_deref(),
            Some("port")
        );
    }

    #[test]
    fn unknown_attribute_in_known_subject_becomes_other() {
        let taxonomy = MemoryTaxonomy::default();
        assert_eq!(
            taxonomy
                .normalize_attribute("database", Some("完全没见过的属性"))
                .as_deref(),
            Some(OTHER_SUBJECT)
        );
    }

    #[test]
    fn config_extends_taxonomy_from_file() {
        let dir = std::env::temp_dir().join(format!(
            "claw-taxonomy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join(TAXONOMY_FILE),
            r#"{
                "subjects": [
                    {"name": "cache", "attributes": ["engine", "ttl"], "aliases": ["缓存"]}
                ],
                "attribute_aliases": [
                    {"subject": "cache", "alias": "过期时间", "canonical": "ttl"}
                ]
            }"#,
        )
        .expect("write config");

        let taxonomy = MemoryTaxonomy::load(&dir);
        assert_eq!(taxonomy.normalize_subject(Some("缓存"), None), "cache");
        assert_eq!(
            taxonomy
                .normalize_attribute("cache", Some("过期时间"))
                .as_deref(),
            Some("ttl")
        );
        // 默认词表仍然可用（追加式合并）。
        assert_eq!(
            taxonomy.normalize_subject(Some("主数据库"), None),
            "database"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn primary_keyword_forces_default_attribute() {
        let taxonomy = MemoryTaxonomy::default();
        let mut op = MemoryOp {
            subject: Some("database".to_string()),
            attribute: Some("engine".to_string()),
            content: "主数据库使用 MySQL 8.0（引擎 MySQL，版本 8.0）".to_string(),
            ..MemoryOp::default()
        };
        taxonomy.normalize_op(&mut op);
        assert_eq!(op.attribute.as_deref(), Some("primary"));

        // 不含主事实关键词的端口事实不被劫持。
        let mut port = MemoryOp {
            subject: Some("database".to_string()),
            attribute: Some("端口".to_string()),
            content: "数据库端口是 5432".to_string(),
            ..MemoryOp::default()
        };
        taxonomy.normalize_op(&mut port);
        assert_eq!(port.attribute.as_deref(), Some("port"));
    }

    #[test]
    fn ascii_keyword_requires_word_boundary() {
        assert!(!contains_keyword("video", "ide"));
        assert!(contains_keyword("use vscode ide", "ide"));
        assert!(contains_keyword("主数据库", "数据库"));
    }
}
