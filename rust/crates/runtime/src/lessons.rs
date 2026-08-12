//! 失败教训持久化(P2:压缩摘要字段化扩展)。
//!
//! # 背景
//!
//! 自进化机制的"失败学习"基于 **turn 级失败**(HarnessArchive Weakness Mining
//! 复用 TraceAnalyzer 失败聚类),成功 turn 中的工具级操作瑕疵(如 git stash
//! 路径前缀事故、read_file 路径错误、权限错误)落在盲区 —— 会话成功结束、
//! 无失败聚类、无错误档案,教训随压缩蒸发,下次任务重蹈覆辙。
//!
//! # 方案(复用既有压缩 LLM 调用,P1 同构)
//!
//! 压缩时本来就要调一次 LLM 生成摘要,让摘要顺带输出 `[lessons]` 段,由摘要
//! 模型从**被压缩历史**中提取失败/低效操作教训(即使整体 turn 成功)。压缩后
//! 解析并追加到 `<workspace>/.claw/lessons.jsonl`;后续请求注入 system 变动区,
//! 让 AI 在下次执行时看到历史教训 → 主动规避。**零额外 LLM 调用**,规则式解析。

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// 教训持久化文件名(位于 `.claw/` 下)。
pub const LESSONS_FILE: &str = "lessons.jsonl";
/// 注入时最多展示的教训条数。
pub const LESSONS_INJECT_MAX: usize = 5;
/// 档案保留的最大教训条数(超出丢弃最旧)。
pub const LESSONS_KEEP_MAX: usize = 30;
/// 单条教训最大字符数。
pub const LESSON_MAX_CHARS: usize = 200;

/// 单条持久化教训。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lesson {
    pub ts_ms: i64,
    pub lesson: String,
}

/// 从压缩摘要解析 `[lessons]` 段(与 `task_state::parse_task_state_from_summary`
/// 同构;摘要为启发式/无 `[lessons]` 段时返回空,调用方跳过)。
#[must_use]
pub fn parse_lessons_from_summary(summary: &str) -> Vec<String> {
    let Some(sec) = extract_section(summary, "[lessons]", "") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in sec.lines() {
        let line = line.trim().trim_start_matches("- ").trim_start_matches("* ");
        if line.is_empty() || line.eq_ignore_ascii_case("none") {
            continue;
        }
        let lesson = truncate(line, LESSON_MAX_CHARS);
        if !lesson.is_empty() && !out.contains(&lesson) {
            out.push(lesson);
        }
    }
    out
}

/// 追加教训到 `<workspace>/.claw/lessons.jsonl`(JSONL,按内容去重,保留最新
/// [`LESSONS_KEEP_MAX`] 条)。返回新增条数;失败返回错误信息(不 panic)。
pub fn append_lessons(root: &Path, lessons: &[String]) -> Result<usize, String> {
    let path = root.join(".claw").join(LESSONS_FILE);
    let mut existing = load_all_lessons(root);
    let now = now_ms();
    let mut added = 0usize;
    for raw in lessons {
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
            continue;
        }
        let lesson = truncate(raw, LESSON_MAX_CHARS);
        if existing.iter().any(|e| e.lesson == lesson) {
            continue;
        }
        existing.push(Lesson {
            ts_ms: now,
            lesson,
        });
        added += 1;
    }
    if added == 0 {
        return Ok(0);
    }
    if existing.len() > LESSONS_KEEP_MAX {
        let excess = existing.len() - LESSONS_KEEP_MAX;
        existing.drain(..excess);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let mut content = String::new();
    for e in &existing {
        content.push_str(&serde_json::to_string(e).map_err(|e| format!("serialize: {e}"))?);
        content.push('\n');
    }
    std::fs::write(&path, content).map_err(|e| format!("write: {e}"))?;
    Ok(added)
}

/// 读取档案中最近 N 条(最新在前)。
#[must_use]
pub fn load_recent_lessons(root: &Path, max: usize) -> Vec<Lesson> {
    let mut all = load_all_lessons(root);
    all.reverse();
    all.truncate(max);
    all
}

/// 渲染为 system prompt 注入块(空时返回空串,调用方跳过注入)。
#[must_use]
pub fn render_for_prompt(lessons: &[Lesson]) -> String {
    if lessons.is_empty() {
        return String::new();
    }
    let mut out = String::from("# 💡 历史操作教训(跨压缩持久化)\n");
    for l in lessons {
        out.push_str(&format!("- {}\n", l.lesson));
    }
    out.push_str("- 注:教训来自历史会话的失败/低效操作,执行同类操作时主动规避。\n");
    out
}

fn load_all_lessons(root: &Path) -> Vec<Lesson> {
    let path = root.join(".claw").join(LESSONS_FILE);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<Lesson>(l).ok())
        .collect()
}

/// 截取 `start_marker` 下一行起、`end_marker` 前的文本段。
/// `end_marker` 为空串时取到文本结尾。
fn extract_section<'a>(text: &'a str, start_marker: &str, end_marker: &str) -> Option<&'a str> {
    let start = text.find(start_marker)?;
    let mut body = &text[start + start_marker.len()..];
    body = body.strip_prefix(':').unwrap_or(body);
    body = body.trim_start_matches(['\r', '\n', ' ']);
    let end = if end_marker.is_empty() {
        body.len()
    } else {
        body.find(end_marker).unwrap_or(body.len())
    };
    Some(&body[..end])
}

/// 截断字符串到指定字符数(按 Unicode 字符)。
fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars - 1).collect();
    format!("{head}…")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_lessons() {
        let summary = "- 修复了登录 401\n\n[lessons]\n- git stash push 需用相对 cwd 的路径, 否则误弹旧 stash\n- read_file 先 ls 确认路径再读, 仓库根可能在上层目录";
        let lessons = parse_lessons_from_summary(summary);
        assert_eq!(lessons.len(), 2);
        assert!(lessons[0].contains("git stash"));
        assert!(lessons[1].contains("read_file"));
    }

    #[test]
    fn parse_handles_none_and_missing_section() {
        assert!(parse_lessons_from_summary("[lessons]\nNONE").is_empty());
        // 启发式摘要无 [lessons] 段
        assert!(parse_lessons_from_summary("- 普通摘要\n- 无教训段").is_empty());
        // 空文本
        assert!(parse_lessons_from_summary("").is_empty());
    }

    #[test]
    fn append_persists_dedupes_and_caps() {
        let tmp = std::env::temp_dir().join(format!("claw-lessons-test-{}", now_ms()));
        let root = Path::new(&tmp);
        std::fs::create_dir_all(root).expect("mkdir");

        // 第一次追加 2 条
        let n = append_lessons(
            root,
            &[
                "git stash 路径事故: 先 git rev-parse --show-toplevel".to_string(),
                "read_file 先确认路径".to_string(),
            ],
        )
        .expect("append");
        assert_eq!(n, 2);

        // 重复追加被去重
        let n2 = append_lessons(root, &["git stash 路径事故: 先 git rev-parse --show-toplevel".to_string()])
            .expect("append dup");
        assert_eq!(n2, 0);

        // 读取最近 1 条(最新在前)
        let recent = load_recent_lessons(root, 1);
        assert_eq!(recent.len(), 1);
        assert!(recent[0].lesson.contains("read_file"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn render_empty_and_nonempty() {
        assert!(render_for_prompt(&[]).is_empty());
        let lessons = vec![Lesson {
            ts_ms: 1,
            lesson: "git stash 用相对路径".to_string(),
        }];
        let rendered = render_for_prompt(&lessons);
        assert!(rendered.contains("历史操作教训"));
        assert!(rendered.contains("git stash 用相对路径"));
    }
}
