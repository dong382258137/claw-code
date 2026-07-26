//! Ported diff hunk builder from grok-build (Apache-2.0).
//!
//! Source: xai-org/grok-build @ a5727c59
//! File:    crates/codegen/xai-grok-pager/src/diff.rs
//! Port scope: 文本 diff 部分(diff_hunks_from_strings / stitch_overlapping_hunks /
//!             diff_hunks_to_patch / DiffLine / DiffHunk)。
//!             跳过 build_diff_hunks / extract_edit_hunks —— 它们依赖
//!             xai_grok_tools::SearchReplaceEditDetail 与 agent_client_protocol,
//!             claw 暂无对应类型;后续若引入 ACP edit detail 再补齐。
//!
//! Adaptation points:
//! - 删除 `use xai_grok_tools::types::output::SearchReplaceEditDetail`
//! - 删除 `build_diff_hunks` / `extract_edit_hunks` 及其测试
//! - 保留 `similar` 外部依赖(Myers 算法),优于 claw 原 tool_card.rs 的逐行比较
//! - `diff_hunks_from_strings` 内部仍构造一个最小 SearchReplaceEditDetail-like
//!   结构,但已内联为本模块私有 `EditDetail`,不引入外部类型

use similar::{ChangeTag, TextDiff};

/// One line of a diff hunk.
///
/// `lo` = old-file line number (1-based); `ln` = new-file line number (1-based)。
/// Delete 行只推进 `lo`,Insert 行只推进 `ln`,Equal 行两者都推进。
#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub text: String,
    pub lo: usize,
    pub ln: usize,
    pub tag: ChangeTag,
}

pub type DiffHunk = Vec<DiffLine>;

/// 内联的最小 edit detail —— Grok Build 原版依赖 `xai_grok_tools::SearchReplaceEditDetail`,
/// claw 没有该类型,这里裁剪为 `diff_hunks_from_strings` 实际用到的字段。
/// 后续若移植 `build_diff_hunks`,需要把 context_before / context_after / line_prefix
/// 字段补回来。
#[derive(Debug, Clone)]
struct EditDetail {
    old_string: String,
    old_line: usize,
    new_string: String,
    new_line: usize,
    context_before: String,
    context_after: String,
    line_prefix: String,
}

/// 从结构化 edit detail 构建 diff hunks(从 Grok Build 原版 `build_diff_hunks` 裁剪)。
///
/// 保留原算法(context_before / context_after / line_prefix / 空行插入启发式 / MAX_CONTEXT 裁剪),
/// 仅替换入参类型为内联 `EditDetail`。
fn build_diff_hunks(details: &[EditDetail]) -> Vec<DiffHunk> {
    const MAX_CONTEXT: usize = 3;
    let mut hunks: Vec<DiffHunk> = Vec::new();

    for edit in details {
        let mut diff_lines: DiffHunk = Vec::new();
        let before_lines: Vec<String> = if edit.context_before.is_empty() {
            vec![]
        } else {
            edit.context_before
                .split_inclusive('\n')
                .map(|s| s.to_string())
                .collect()
        };
        let n_before = before_lines.len();
        for (i, line_text) in before_lines.into_iter().enumerate() {
            let from_end = n_before.saturating_sub(i + 1);
            let lo = edit.old_line.saturating_sub(from_end + 1);
            let ln = edit.new_line.saturating_sub(from_end + 1);
            diff_lines.push(DiffLine {
                text: line_text,
                lo,
                ln,
                tag: ChangeTag::Equal,
            });
        }
        let (mut lo, mut ln) = (edit.old_line, edit.new_line);
        let empty_to_empty = edit.old_string.is_empty() && edit.new_string.is_empty();
        let mid_file = !edit.context_before.is_empty() || !edit.context_after.is_empty();
        let new_text: &str = if empty_to_empty && mid_file {
            "\n"
        } else {
            &edit.new_string
        };
        let prefix = &edit.line_prefix;
        let has_prefix = !prefix.is_empty();
        let mut prefix_applied_delete = false;
        let mut prefix_applied_insert = false;
        let diff = TextDiff::from_lines(edit.old_string.as_str(), new_text);
        for change in diff.iter_all_changes() {
            let tag = change.tag();
            let mut text = change.value().to_owned();
            if has_prefix {
                let needs_prefix = match tag {
                    ChangeTag::Delete => !prefix_applied_delete,
                    ChangeTag::Insert => !prefix_applied_insert,
                    ChangeTag::Equal => !prefix_applied_delete && !prefix_applied_insert,
                };
                if needs_prefix {
                    text.insert_str(0, prefix);
                }
                match tag {
                    ChangeTag::Delete | ChangeTag::Equal => prefix_applied_delete = true,
                    ChangeTag::Insert => prefix_applied_insert = true,
                }
            }
            diff_lines.push(DiffLine { text, lo, ln, tag });
            match tag {
                ChangeTag::Equal => {
                    lo = lo.saturating_add(1);
                    ln = ln.saturating_add(1);
                }
                ChangeTag::Delete => {
                    lo = lo.saturating_add(1);
                }
                ChangeTag::Insert => {
                    ln = ln.saturating_add(1);
                }
            }
        }

        if !edit.context_after.is_empty() {
            for line in edit.context_after.split_inclusive('\n') {
                diff_lines.push(DiffLine {
                    text: line.to_owned(),
                    lo,
                    ln,
                    tag: ChangeTag::Equal,
                });
                lo = lo.saturating_add(1);
                ln = ln.saturating_add(1);
            }
        }

        let total_len = diff_lines.len();
        let mut start;
        let mut end = total_len;
        if diff_lines.iter().all(|entry| entry.tag == ChangeTag::Equal) {
            start = end;
        } else {
            let equal_before = diff_lines
                .iter()
                .take_while(|entry| entry.tag == ChangeTag::Equal)
                .count();
            let equal_after = diff_lines
                .iter()
                .rev()
                .take_while(|entry| entry.tag == ChangeTag::Equal)
                .count();
            start = equal_before.saturating_sub(MAX_CONTEXT);
            end = total_len.saturating_sub(equal_after.saturating_sub(MAX_CONTEXT));
        }

        while start < end {
            let entry = &diff_lines[start];
            if entry.tag == ChangeTag::Equal && entry.text.trim_ascii().is_empty() {
                start += 1;
            } else {
                break;
            }
        }
        while start < end {
            let entry = &diff_lines[end - 1];
            if entry.tag == ChangeTag::Equal && entry.text.trim_ascii().is_empty() {
                end -= 1;
            } else {
                break;
            }
        }

        if start < end {
            hunks.push(diff_lines[start..end].to_vec());
        }
    }
    hunks
}

/// 从完整 old/new 文本构建 diff hunks。
///
/// 这是 claw 集成的主入口:tool_card.rs 的 `render_edit_diff` 当前只有
/// 逐行比较,改用本函数后获得 Myers 算法的语义化 diff。
pub fn diff_hunks_from_strings(old_text: &str, new_text: &str, start_line: usize) -> Vec<DiffHunk> {
    let detail = EditDetail {
        old_string: old_text.to_owned(),
        old_line: start_line,
        new_string: new_text.to_owned(),
        new_line: start_line,
        context_before: String::new(),
        context_after: String::new(),
        line_prefix: String::new(),
    };
    build_diff_hunks(&[detail])
}

/// 合并重叠/相邻 hunks(Grok Build 原版 `stitch_overlapping_hunks`)。
///
/// 同一文件多次编辑各自带 ±context,合并后避免重复显示上下文行。
pub fn stitch_overlapping_hunks(hunks: Vec<DiffHunk>) -> Vec<DiffHunk> {
    let mut out: Vec<DiffHunk> = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        // 注:Grok Build 原版用 `if let ... && let ...` let chain(Rust 2024),
        // claw edition 更早,改写为嵌套 if let。
        let mut stitched = false;
        if let Some(last) = out.last_mut() {
            if let Some(s) = stitch_hunk_pair(last, &hunk) {
                *last = s;
                stitched = true;
            }
        }
        if stitched {
            continue;
        }
        out.push(hunk);
    }
    out
}

fn render_range(hunk: &DiffHunk) -> Option<(usize, usize)> {
    let mut range: Option<(usize, usize)> = None;
    for line in hunk {
        if line.tag == ChangeTag::Delete {
            continue;
        }
        range = Some(match range {
            None => (line.ln, line.ln),
            Some((min, max)) => (min.min(line.ln), max.max(line.ln)),
        });
    }
    range
}

fn render_pos(hunk: &DiffHunk, ln: usize) -> Option<usize> {
    hunk.iter()
        .position(|l| l.tag != ChangeTag::Delete && l.ln == ln)
}

fn trimmed(text: &str) -> &str {
    text.trim_end_matches(['\r', '\n'])
}

fn stitch_hunk_pair(a: &DiffHunk, b: &DiffHunk) -> Option<DiffHunk> {
    let (a_min, a_max) = render_range(a)?;
    let (b_min, _) = render_range(b)?;
    if b_min < a_min || b_min > a_max + 1 {
        return None;
    }

    let mut out = a.clone();
    let mut max_ln = a_max;
    let mut i = 0;
    while i < b.len() {
        let row = &b[i];
        if row.ln > max_ln {
            for rest in &b[i..] {
                if rest.tag != ChangeTag::Delete {
                    if rest.ln != max_ln + 1 {
                        return None;
                    }
                    max_ln = rest.ln;
                }
                out.push(rest.clone());
            }
            break;
        }
        match row.tag {
            ChangeTag::Equal => {
                let pos = render_pos(&out, row.ln)?;
                if trimmed(&out[pos].text) != trimmed(&row.text) {
                    return None;
                }
                i += 1;
            }
            ChangeTag::Delete => {
                let next = b.get(i + 1)?;
                if next.tag != ChangeTag::Insert || next.ln != row.ln {
                    return None;
                }
                let pos = render_pos(&out, row.ln)?;
                if trimmed(&out[pos].text) != trimmed(&row.text) {
                    return None;
                }
                match out[pos].tag {
                    ChangeTag::Equal => {
                        out[pos] = row.clone();
                        out.insert(pos + 1, next.clone());
                    }
                    ChangeTag::Insert => {
                        out[pos] = next.clone();
                    }
                    ChangeTag::Delete => unreachable!("render_pos skips deletes"),
                }
                i += 2;
            }
            ChangeTag::Insert => return None,
        }
    }
    Some(out)
}

/// 生成 unified diff patch 字符串(可用于 `git apply` 或剪贴板分享)。
pub fn diff_hunks_to_patch(path: &str, hunks: &[DiffHunk]) -> String {
    if hunks.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));

    for hunk in hunks {
        if hunk.is_empty() {
            continue;
        }
        let old_start = hunk
            .iter()
            .filter(|l| l.tag != ChangeTag::Insert)
            .map(|l| l.lo)
            .next()
            .unwrap_or(1);
        let new_start = hunk
            .iter()
            .filter(|l| l.tag != ChangeTag::Delete)
            .map(|l| l.ln)
            .next()
            .unwrap_or(1);
        let old_count = hunk.iter().filter(|l| l.tag != ChangeTag::Insert).count();
        let new_count = hunk.iter().filter(|l| l.tag != ChangeTag::Delete).count();

        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));

        for line in hunk {
            let prefix = match line.tag {
                ChangeTag::Equal => ' ',
                ChangeTag::Insert => '+',
                ChangeTag::Delete => '-',
            };
            let text = line.text.trim_end_matches(['\r', '\n']);
            out.push(prefix);
            out.push_str(text);
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_hunks_from_strings_simple() {
        let hunks = diff_hunks_from_strings("hello\nworld\n", "hello\nearth\n", 1);
        assert_eq!(hunks.len(), 1);

        let hunk = &hunks[0];
        let deletes: Vec<_> = hunk.iter().filter(|l| l.tag == ChangeTag::Delete).collect();
        let inserts: Vec<_> = hunk.iter().filter(|l| l.tag == ChangeTag::Insert).collect();
        assert_eq!(deletes.len(), 1);
        assert_eq!(inserts.len(), 1);
        assert!(deletes[0].text.contains("world"));
        assert!(inserts[0].text.contains("earth"));
    }

    #[test]
    fn diff_hunks_from_strings_identical() {
        let hunks = diff_hunks_from_strings("same\n", "same\n", 1);
        assert_eq!(hunks.len(), 0);
    }

    #[test]
    fn diff_hunks_from_strings_empty_old() {
        let hunks = diff_hunks_from_strings("", "new content\n", 1);
        assert_eq!(hunks.len(), 1);
        let inserts: Vec<_> = hunks[0]
            .iter()
            .filter(|l| l.tag == ChangeTag::Insert)
            .collect();
        assert!(!inserts.is_empty());
    }

    #[test]
    fn empty_to_empty_produces_no_hunks() {
        let hunks = diff_hunks_from_strings("", "", 1);
        assert!(hunks.is_empty());
    }

    #[test]
    fn patch_format_has_hunk_header() {
        let hunks = diff_hunks_from_strings("old\n", "new\n", 5);
        let patch = diff_hunks_to_patch("test.rs", &hunks);
        assert!(patch.contains("--- a/test.rs"));
        assert!(patch.contains("+++ b/test.rs"));
        assert!(patch.contains("@@ -5,"));
        assert!(patch.contains("-old"));
        assert!(patch.contains("+new"));
    }

    #[test]
    fn stitch_collapses_double_edit_to_original_and_final() {
        let first = build_diff_hunks(&[EditDetail {
            old_string: "a".to_string(),
            new_string: "b".to_string(),
            old_line: 1,
            new_line: 1,
            context_before: String::new(),
            context_after: "x\n".to_string(),
            line_prefix: String::new(),
        }]);
        let second = build_diff_hunks(&[EditDetail {
            old_string: "b".to_string(),
            new_string: "c".to_string(),
            old_line: 1,
            new_line: 1,
            context_before: String::new(),
            context_after: "x\n".to_string(),
            line_prefix: String::new(),
        }]);

        let stitched = stitch_overlapping_hunks(vec![first[0].clone(), second[0].clone()]);
        assert_eq!(stitched.len(), 1);
        let rows: Vec<(ChangeTag, usize, &str)> = stitched[0]
            .iter()
            .map(|l| (l.tag, l.ln, l.text.trim_end()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (ChangeTag::Delete, 1, "a"),
                (ChangeTag::Insert, 1, "c"),
                (ChangeTag::Equal, 2, "x"),
            ]
        );
    }

    #[test]
    fn stitch_keeps_disjoint_hunks_separate() {
        let far = |line: usize| {
            diff_hunks_from_strings(&format!("old_{line}\n"), &format!("new_{line}\n"), line)
                .remove(0)
        };
        assert_eq!(stitch_overlapping_hunks(vec![far(5), far(40)]).len(), 2);
    }
}
