//! Ported project picker from grok-build (Apache-2.0).
//!
//! Source: xai-org/grok-build @ d02693a8
//! File:    crates/codegen/xai-grok-pager/src/project_picker/{mod.rs, sources.rs}
//! Port scope: 数据模型(ProjectQuestion/Question/QuestionOption)、
//!             build_project_question(问题构建器)、
//!             is_project_dir(项目目录检测)、display_path(路径展示)
//!             跳过 collect_recent_dirs —— 依赖 xai_grok_shell::session::persistence,
//!             claw 无对应;改由调用方直接传入 (PathBuf, DateTime) 切片。
//!
//! Adaptation points:
//! - `dirs::home_dir()` → `home_dir()` 内联(claw 模式:USERPROFILE/HOME 环境变量)
//! - `xai_file_utils::workspace_classifier::is_project_dir` → 内联实现(检查常见项目标记文件)
//! - `xai_grok_tools::ask_user_question::{Question, QuestionOption}` → 内联类型
//! - `crate::render::line_utils::truncate_str` → 内联实现
//! - `crate::views::session_title::format_relative_time` → 内联实现
//! - `xai_grok_shell::session::persistence::list_recent_summaries` → 删除,改由调用方传入

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

// ── 内联类型(替代 xai_grok_tools::ask_user_question) ───────────────

/// 一个用户可选择的选项。
#[derive(Debug, Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
    pub id: Option<String>,
}

/// 一个呈现给用户的问题(单选/多选)。
#[derive(Debug, Clone)]
pub struct Question {
    pub question: String,
    pub id: Option<String>,
    pub options: Vec<QuestionOption>,
    pub multi_select: Option<bool>,
}

// ── 项目目录检测(替代 xai_file_utils::workspace_classifier) ─────────

/// 判断一个目录是否是"项目目录"(含常见项目标记文件)。
///
/// 检查以下标记文件/目录(任一存在即判定为项目目录):
/// `.git` / `Cargo.toml` / `package.json` / `pyproject.toml` /
/// `go.mod` / `.hg` / `pom.xml` / `build.gradle` / `build.gradle.kts`
pub fn is_project_dir(path: &Path) -> bool {
    const MARKERS: &[&str] = &[
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        ".hg",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
    ];
    MARKERS.iter().any(|m| path.join(m).exists())
}

// ── 路径展示工具(替代 crate::render / sources::display_path) ─────────

/// 获取 home 目录(claw 模式:优先 USERPROFILE,回退 HOME)。
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// 把路径展示为 `~/relative` 形式(home 目录折叠为 ~)。
pub fn display_path(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

// ── 字符串工具(替代 crate::render::line_utils::truncate_str) ────────

/// 把字符串截断到 max_chars 字符宽度,超出时末尾加 `…`。
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}…")
}

// ── 时间格式化(替代 crate::views::session_title::format_relative_time) ─

/// 把持续时间格式化为相对时间字符串("3m ago"、"2h ago"、"5d ago")。
fn format_relative_time(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

// ── 核心问题构建器 ───────────────────────────────────────────────────

/// `resolved_paths` 与 `question.options` 前部索引对齐。
/// 尾部的 "Don't ask me again" 选项(dont_ask_index)没有对应路径
/// (选中它则继续在当前目录)。
pub struct ProjectQuestion {
    pub question: Question,
    pub resolved_paths: Vec<PathBuf>,
    /// "Don't ask me again" 选项的索引。
    pub dont_ask_index: usize,
}

const MAX_RECENT_DIRS: usize = 5;

/// 构建项目选择问题。
///
/// `recent_dirs` 由调用方提供(claw 可从 session 历史或配置文件收集),
/// 格式为 `(路径, 最后使用时间)` 切片,按时间倒序。
pub fn build_project_question(
    recent_dirs: &[(PathBuf, DateTime<Utc>)],
    cwd: &Path,
) -> ProjectQuestion {
    let mut options = Vec::new();
    let mut resolved_paths = Vec::new();

    // 第一项:继续在当前目录
    let is_home = home_dir().is_some_and(|h| h == cwd);
    let cwd_name = if is_home {
        "~"
    } else {
        cwd.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("current directory")
    };
    options.push(QuestionOption {
        label: format!("{cwd_name} (current)"),
        description: display_path(cwd),
        preview: None,
        id: None,
    });
    resolved_paths.push(cwd.to_path_buf());

    // 最近项目目录
    for (path, ts) in recent_dirs
        .iter()
        .filter(|(p, _)| p != cwd)
        .take(MAX_RECENT_DIRS)
    {
        let raw_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let name = truncate_str(raw_name, 22);
        options.push(QuestionOption {
            label: name,
            description: format!(
                "{}  ({})",
                display_path(path),
                format_relative_time((Utc::now() - *ts).to_std().unwrap_or_default())
            ),
            preview: None,
            id: None,
        });
        resolved_paths.push(path.clone());
    }

    // 尾部 "Don't ask me again" 选项,不进 resolved_paths(保持索引对齐)
    let dont_ask_index = options.len();
    options.push(QuestionOption {
        label: "Don't ask me again".to_string(),
        description: "Always start in the current directory (reset in config.toml)".to_string(),
        preview: None,
        id: None,
    });

    ProjectQuestion {
        question: Question {
            question: "Run claw in a project directory?\n\n\
                 This gives claw full context of your codebase for better results."
                .into(),
            id: None,
            options,
            multi_select: Some(false),
        },
        resolved_paths,
        dont_ask_index,
    }
}

// ── stdout 渲染 + 输入解析(用于 TUI 启动前的交互式菜单) ─────────────

/// 把 `ProjectQuestion` 渲染为 stdout 菜单文本。
///
/// 格式:
/// ```text
/// Run claw in a project directory?
///
/// This gives claw full context of your codebase for better results.
///
///   1) claw (current)           d:/claw-code-src
///   2) alpha                     ~/projects/alpha  (3m ago)
///   ...
///   N) Don't ask me again
///
/// Select [1-N]:
/// ```
pub fn render_question_stdout(q: &ProjectQuestion) -> String {
    let mut out = String::new();
    out.push_str(&q.question.question);
    out.push_str("\n\n");
    for (i, opt) in q.question.options.iter().enumerate() {
        let idx = i + 1;
        if opt.description.is_empty() {
            out.push_str(&format!("  {idx}) {}\n", opt.label));
        } else {
            out.push_str(&format!(
                "  {idx}) {:<26}  {}\n",
                opt.label, opt.description
            ));
        }
    }
    out.push_str("\nSelect [1-N]: ");
    out
}

/// 解析用户输入,返回选择的路径(如果有)。
///
/// 返回值:
/// - `Some(Some(path))`:用户选择了一个项目目录,切换到该路径
/// - `Some(None)`:用户选择了 "Don't ask me again",保持当前目录且不再询问
/// - `None`:输入无效,调用方应重新提示
pub fn parse_choice<'a>(q: &'a ProjectQuestion, input: &str) -> Option<Option<&'a Path>> {
    let trimmed = input.trim();
    let n: usize = trimmed.parse().ok()?;
    if n == 0 || n > q.question.options.len() {
        return None;
    }
    let idx = n - 1;
    if idx == q.dont_ask_index {
        Some(None) // Don't ask me again
    } else {
        q.resolved_paths.get(idx).map(|p| Some(p.as_path()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn no_recent_dirs_returns_only_cwd() {
        let pq = build_project_question(&[], Path::new("/home/user"));
        assert_eq!(pq.resolved_paths.len(), 1);
        assert_eq!(pq.resolved_paths[0], PathBuf::from("/home/user"));
    }

    #[test]
    fn recent_dirs_index_aligned_with_options() {
        let now = Utc::now();
        let recent = vec![
            (PathBuf::from("/projects/alpha"), now),
            (PathBuf::from("/projects/beta"), now),
        ];
        let pq = build_project_question(&recent, Path::new("/home/user"));
        // Options 比 resolved_paths 多一个尾部 "Don't ask me again"
        assert_eq!(pq.question.options.len(), pq.resolved_paths.len() + 1);
        assert_eq!(pq.resolved_paths[0], PathBuf::from("/home/user"));
        assert_eq!(pq.resolved_paths[1], PathBuf::from("/projects/alpha"));
        assert_eq!(pq.resolved_paths[2], PathBuf::from("/projects/beta"));
    }

    #[test]
    fn dont_ask_option_is_last_and_excluded_from_paths() {
        let pq = build_project_question(&[], Path::new("/home/user"));
        assert_eq!(pq.dont_ask_index, pq.resolved_paths.len());
        assert_eq!(pq.dont_ask_index, pq.question.options.len() - 1);
        assert_eq!(
            pq.question.options[pq.dont_ask_index].label,
            "Don't ask me again"
        );
    }

    #[test]
    fn cwd_filtered_from_recent_dirs() {
        // cwd 不应在 recent_dirs 中重复出现
        let now = Utc::now();
        let recent = vec![(PathBuf::from("/home/user"), now)];
        let pq = build_project_question(&recent, Path::new("/home/user"));
        assert_eq!(pq.resolved_paths.len(), 1); // 只有 cwd, recent 被过滤
    }

    #[test]
    fn max_recent_dirs_enforced() {
        let now = Utc::now();
        let recent: Vec<(PathBuf, DateTime<Utc>)> = (0..10)
            .map(|i| (PathBuf::from(format!("/projects/p{i}")), now))
            .collect();
        let pq = build_project_question(&recent, Path::new("/home/user"));
        // cwd + MAX_RECENT_DIRS 个 recent
        assert_eq!(pq.resolved_paths.len(), 1 + MAX_RECENT_DIRS);
    }

    #[test]
    fn display_path_collapses_home() {
        // 仅测试格式逻辑,不依赖实际 home_dir 是否存在
        // display_path("/foo/bar") 应返回 "/foo/bar"(非 home 前缀)
        assert_eq!(display_path(Path::new("/foo/bar")), "/foo/bar");
    }

    #[test]
    fn truncate_str_short_unchanged() {
        assert_eq!(truncate_str("abc", 10), "abc");
    }

    #[test]
    fn truncate_str_long_truncated() {
        let result = truncate_str("abcdefghij", 5);
        assert_eq!(result, "abcde…");
    }

    #[test]
    fn format_relative_time_seconds() {
        assert_eq!(
            format_relative_time(std::time::Duration::from_secs(30)),
            "30s ago"
        );
    }

    #[test]
    fn format_relative_time_minutes() {
        assert_eq!(
            format_relative_time(std::time::Duration::from_secs(180)),
            "3m ago"
        );
    }

    #[test]
    fn format_relative_time_hours() {
        assert_eq!(
            format_relative_time(std::time::Duration::from_secs(7200)),
            "2h ago"
        );
    }

    #[test]
    fn format_relative_time_days() {
        assert_eq!(
            format_relative_time(std::time::Duration::from_secs(86400 * 5)),
            "5d ago"
        );
    }

    #[test]
    fn is_project_dir_detects_git() {
        // 用 tempdir 创建临时目录测试
        let tmp = std::env::temp_dir().join("claw_test_project_picker_git");
        let _ = std::fs::create_dir_all(&tmp);
        let git_dir = tmp.join(".git");
        let _ = std::fs::create_dir_all(&git_dir);
        assert!(is_project_dir(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn is_project_dir_detects_cargo() {
        let tmp = std::env::temp_dir().join("claw_test_project_picker_cargo");
        let _ = std::fs::create_dir_all(&tmp);
        let _ = std::fs::write(tmp.join("Cargo.toml"), "");
        assert!(is_project_dir(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn is_project_dir_rejects_empty() {
        let tmp = std::env::temp_dir().join("claw_test_project_picker_empty");
        let _ = std::fs::create_dir_all(&tmp);
        assert!(!is_project_dir(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn render_question_stdout_includes_question_and_options() {
        let now = Utc::now();
        let recent = vec![(PathBuf::from("/projects/alpha"), now)];
        let pq = build_project_question(&recent, Path::new("/home/user"));
        let rendered = render_question_stdout(&pq);
        assert!(rendered.contains("Run claw in a project directory?"));
        assert!(rendered.contains("1)"));
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("Don't ask me again"));
        assert!(rendered.contains("Select [1-N]:"));
    }

    #[test]
    fn parse_choice_valid_path_selection() {
        let now = Utc::now();
        let recent = vec![(PathBuf::from("/projects/alpha"), now)];
        let pq = build_project_question(&recent, Path::new("/home/user"));
        // 选 1 = cwd
        assert_eq!(parse_choice(&pq, "1"), Some(Some(Path::new("/home/user"))));
        // 选 2 = alpha
        assert_eq!(
            parse_choice(&pq, "2"),
            Some(Some(Path::new("/projects/alpha")))
        );
    }

    #[test]
    fn parse_choice_dont_ask_returns_none_inner() {
        let pq = build_project_question(&[], Path::new("/home/user"));
        // 最后一个选项是 "Don't ask me again"
        let last = pq.question.options.len();
        assert_eq!(parse_choice(&pq, &last.to_string()), Some(None));
    }

    #[test]
    fn parse_choice_invalid_returns_none() {
        let pq = build_project_question(&[], Path::new("/home/user"));
        assert_eq!(parse_choice(&pq, "0"), None); // 0 无效
        assert_eq!(parse_choice(&pq, "99"), None); // 超出范围
        assert_eq!(parse_choice(&pq, "abc"), None); // 非数字
    }
}
