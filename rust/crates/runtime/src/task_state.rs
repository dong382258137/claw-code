//! 任务状态(Task State)自动维护 —— 长链任务的任务锚点(episodic memory)。
//!
//! # 背景
//!
//! 长链任务(数十上百轮工具调用)中,AI 的"当前任务是什么、进度到哪、已确认
//! 哪些关键发现"只存在于会话历史里。一旦上下文被压缩,任务锚点随之蒸发,
//! 表现为:
//! - AI 丢失任务身份,反复 `session_search` 找回"这个细节服务于什么任务"
//! - 已确认的根因/结论被压缩,AI 只能重新调用工具查询(token 浪费)
//! - 压缩 summary 的"继续旧任务"指令导致任务漂移
//!
//! 本模块提供**自动**维护(不依赖 AI 主动调用 `notebook_update`):每个 turn
//! 结束时由 runtime 规则式提取 goal + findings 并持久化到
//! `<workspace>/.claw/task_state.json`;会话经历过压缩时,每次请求注入到
//! system 变动区,让 AI 在压缩后仍持有任务锚点。
//!
//! 依据:MAGE "Memory as Execution State Management"(arXiv:2606.06090)、
//! Anthropic《Effective Context Engineering》(memory tool + structured notes)。
//! 规则式提取,零 LLM 成本。

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// 任务目标的最大字符数(恒定体积,避免无限增长)。
pub const TASK_GOAL_MAX_CHARS: usize = 150;
/// 单条关键发现的最大字符数。
pub const TASK_FINDING_MAX_CHARS: usize = 120;
/// findings 容量上限(保留最新 N 条)。
pub const TASK_FINDINGS_MAX: usize = 6;
/// task_state 持久化文件名(位于 `.claw/` 下)。
pub const TASK_STATE_FILE: &str = "task_state.json";
/// 判定"用户换新任务"的 goal 前缀比较长度。
const GOAL_PREFIX_COMPARE_CHARS: usize = 30;
/// 用户输入短于此字符数时不视为新任务描述(如"继续"/"好"),保留旧 goal。
const MIN_GOAL_INPUT_CHARS: usize = 8;

/// 过程性/行动性开头词 —— 以这些开头的行描述的是"正在做什么/计划做什么"
/// (如"开始调查根因"),不是已确认的结论,不应进入 findings。
/// 防过程噪声污染 findings(小写匹配,英文带尾空格)。
const PROCESS_START_WORDS: &[&str] = &[
    "开始",
    "继续",
    "正在",
    "尝试",
    "进行",
    "准备",
    "接着",
    "现在",
    "先",
    "要",
    "需要",
    "我想",
    "我尝试",
    "我计划",
    "让我",
    "我来",
    "调查根因",
    "查找原因",
    "排查原因",
    "定位原因",
    "分析原因",
    "了解原因",
    "找原因",
    "查原因",
    "查明原因",
    "let me ",
    "i will ",
    "i'll ",
    "i am ",
    "i'm ",
    "going to ",
    "trying to ",
    "start ",
    "investigate ",
    "looking for the ",
    "find the root cause",
];

/// 关键发现信号词 —— 命中任一的行被视为"已确认的关键发现"。
const FINDING_KEYWORDS: &[&str] = &[
    "根因",
    "结论",
    "关键",
    "发现",
    "原因",
    "修复",
    "确认",
    "已验证",
    "PASS",
    "FAIL",
    "root cause",
    "conclusion",
    "verified",
    "found that",
    "fix",
];

/// 结论陈述强标记 —— 无冒号的陈述若含以下任一,也视为已确认结论
/// (如"根因是缓存失效""测试 PASS")。仅命中关键词但既无冒号也无强标记
/// 的行(如"正在讨论根因的可能性")不进入 findings,防止状态被噪声污染。
const FINDING_STRONG_MARKERS: &[&str] = &[
    "根因是",
    "原因是",
    "确认",
    "已验证",
    "结论",
    "已修复",
    "修复了",
    "PASS",
    "FAIL",
    "found that",
    "verified",
];

/// 任务状态:当前目标 + 已确认的关键发现 + 已收尾任务。
///
/// 这是"执行状态"(episodic)而非语义事实(semantic),与 memory.rs 的
/// PersistentMemory 互补:后者存用户偏好/纠正,本对象存任务进行状态。
/// `closed_tasks` 由压缩摘要的 `[closed_tasks]` 段解析而来(P1),用于防止
/// 压缩指令残留把 AI 拉回已收尾任务。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TaskState {
    pub goal: String,
    pub findings: Vec<String>,
    #[serde(default)]
    pub closed_tasks: Vec<String>,
    /// 第2项:已完成的子目标(对齐 PlanArtifact 的 `StepStatus::Succeeded`)。
    /// 跨压缩/重开持久化,让 AI 从最近成功 step 续跑,不重跑已完成步骤。
    #[serde(default)]
    pub completed_subgoals: Vec<String>,
    pub updated_at_ms: i64,
}

impl TaskState {
    /// 从 `<workspace>/.claw/task_state.json` 加载,失败/不存在返回 None。
    #[must_use]
    pub fn load(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// 持久化到磁盘。失败返回错误信息(不 panic,不阻断主流程)。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let content = serde_json::to_string_pretty(self).map_err(|e| format!("serialize: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        std::fs::write(path, content).map_err(|e| format!("write: {e}"))
    }

    /// 从本 turn 规则式更新任务状态(零 LLM 成本)。
    ///
    /// - `user_input`:本 turn 的用户消息。长度 >= [`MIN_GOAL_INPUT_CHARS`] 时
    ///   视为任务描述;与旧 goal 前缀不同则判定为换任务,清空旧 findings。
    /// - `assistant_texts`:本 turn 的 assistant 文本块。逐行扫描含
    ///   [`FINDING_KEYWORDS`] 信号词的行,去重后追加为新发现(保留最新 N 条)。
    pub fn update_from_turn(&mut self, user_input: &str, assistant_texts: &[String]) {
        let trimmed = user_input.trim();
        let goal = truncate(trimmed, TASK_GOAL_MAX_CHARS);
        if !goal.is_empty() && trimmed.chars().count() >= MIN_GOAL_INPUT_CHARS {
            if !is_same_task(&self.goal, &goal) {
                self.findings.clear();
            }
            self.goal = goal;
        }

        for text in assistant_texts {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // 排除过程性/行动性行("开始调查根因"),只保留结论陈述
                let lower = line.to_lowercase();
                let is_process = PROCESS_START_WORDS.iter().any(|w| lower.starts_with(w));
                if is_process {
                    continue;
                }
                if FINDING_KEYWORDS.iter().any(|k| line.contains(k)) {
                    // 结论门槛:仅命中关键词不够,必须是"结论陈述"——
                    // 含冒号(如"根因:缓存失效")或含强判定标记(如"根因是缓存失效")。
                    let is_conclusion = line.contains(':')
                        || FINDING_STRONG_MARKERS.iter().any(|m| line.contains(m));
                    if !is_conclusion {
                        continue;
                    }
                    let finding = truncate(line, TASK_FINDING_MAX_CHARS);
                    if !finding.is_empty() && !self.findings.contains(&finding) {
                        self.findings.push(finding);
                    }
                }
            }
        }
        if self.findings.len() > TASK_FINDINGS_MAX {
            let excess = self.findings.len() - TASK_FINDINGS_MAX;
            self.findings.drain(..excess);
        }
        self.updated_at_ms = now_ms();
    }

    /// 第2项:合并已完成子目标(去重 + 截断 + 上限),供 plan Review 阶段同步。
    ///
    /// 子目标来自 PlanArtifact 的 `StepStatus::Succeeded` step 描述,跨压缩/
    /// 重开持久化,让 AI 从最近成功 step 续跑,不重跑已完成步骤。
    pub fn record_completed_subgoals(&mut self, subgoals: &[String]) {
        for g in subgoals {
            if self.completed_subgoals.len() >= TASK_FINDINGS_MAX {
                break;
            }
            let g = truncate(g.trim(), TASK_FINDING_MAX_CHARS);
            if g.is_empty() || self.completed_subgoals.contains(&g) {
                continue;
            }
            self.completed_subgoals.push(g);
        }
    }

    /// 渲染为 system prompt 注入块(精简,恒定体积)。
    ///
    /// 空状态返回空串(调用方跳过注入,不增加 token 开销)。
    #[must_use]
    pub fn render_for_prompt(&self) -> String {
        if self.goal.is_empty() && self.findings.is_empty() && self.completed_subgoals.is_empty() {
            return String::new();
        }
        let mut out = String::from("# 📌 当前任务状态(跨压缩持久化)\n");
        if !self.goal.is_empty() {
            out.push_str(&format!("- 目标: {}\n", self.goal));
        }
        if !self.findings.is_empty() {
            out.push_str("- 已确认的关键发现:\n");
            for f in &self.findings {
                out.push_str(&format!("  · {f}\n"));
            }
        }
        if !self.completed_subgoals.is_empty() {
            out.push_str("- 已完成的子目标(勿重复执行):\n");
            for g in &self.completed_subgoals {
                out.push_str(&format!("  · {g}\n"));
            }
        }
        if !self.closed_tasks.is_empty() {
            out.push_str("- 已收尾任务(禁止继续,即使摘要中描述详细):\n");
            for t in &self.closed_tasks {
                out.push_str(&format!("  · {t}\n"));
            }
        }
        // 可信度注脚:该状态由 runtime 规则自动记录,可能存在误提取;
        // 以对话实际内容为准,防止错误状态误导模型。
        out.push_str("- 注:此状态由 runtime 自动记录,如与当前对话不符,请以最新对话为准。\n");
        out
    }
}

/// 判定新旧 goal 是否属于同一任务:比较前 [`GOAL_PREFIX_COMPARE_CHARS`] 字符前缀。
fn is_same_task(prev: &str, new: &str) -> bool {
    let p: String = prev.chars().take(GOAL_PREFIX_COMPARE_CHARS).collect();
    let n: String = new.chars().take(GOAL_PREFIX_COMPARE_CHARS).collect();
    if p.is_empty() || n.is_empty() {
        return false;
    }
    p == n || n.starts_with(&p) || p.starts_with(&n)
}

/// 截断字符串到指定字符数(按 Unicode 字符,不按字节)。
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

/// 从压缩摘要中解析出的任务状态(P1:压缩摘要字段化)。
///
/// 压缩时本来就要调一次 LLM 生成摘要(claw 的 `CompactionSummarizerClient`),
/// 让摘要按模板输出 `[active_task]` / `[closed_tasks]` 结构化段,压缩后从
/// 摘要文本解析出当前任务与已收尾任务 —— **零额外 LLM 调用**(复用既有压缩
/// 调用,对齐 Claude Code 的 9 部分结构化摘要与 Codex Memories 的提取管线)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryTaskExtract {
    /// `[active_task]` 段的 `goal:` 字段(排除 NONE)。
    pub active_goal: Option<String>,
    /// `[active_task]` 段的 `next_action:` 字段(排除 NONE)。
    pub next_action: Option<String>,
    /// `[closed_tasks]` 段的条目(排除 NONE/空行,截断去重,上限 N 条)。
    pub closed_tasks: Vec<String>,
}

/// 解析压缩摘要中的结构化任务段。摘要无 `[active_task]` 段(如启发式摘要)
/// 时返回空提取,调用方跳过更新。
#[must_use]
pub fn parse_task_state_from_summary(summary: &str) -> SummaryTaskExtract {
    let mut out = SummaryTaskExtract::default();

    // [active_task] 段:从标记下一行到 [closed_tasks] 段(或结尾)。
    if let Some(sec) = extract_section(summary, "[active_task]", "[closed_tasks]") {
        for line in sec.lines() {
            let line = line.trim();
            if let Some(v) = strip_field_prefix(line, "goal") {
                if !is_none_value(&v) {
                    out.active_goal = Some(truncate(&v, TASK_GOAL_MAX_CHARS));
                }
            } else if let Some(v) = strip_field_prefix(line, "next_action") {
                if !is_none_value(&v) {
                    out.next_action = Some(truncate(&v, TASK_FINDING_MAX_CHARS));
                }
            }
        }
    }

    // [closed_tasks] 段:到结尾。
    if let Some(sec) = extract_section(summary, "[closed_tasks]", "") {
        for line in sec.lines() {
            let line = line
                .trim()
                .trim_start_matches("- ")
                .trim_start_matches("* ");
            if line.is_empty() || is_none_value(line) || !is_task_completion_like(line) {
                continue;
            }
            let t = truncate(line, TASK_FINDING_MAX_CHARS);
            if !out.closed_tasks.contains(&t) {
                out.closed_tasks.push(t);
            }
            if out.closed_tasks.len() >= TASK_FINDINGS_MAX {
                break;
            }
        }
    }
    out
}

/// C3 防护:判定一行是否像"已完成任务声明",而非 LLM 字段混淆产物。
///
/// 压缩摘要的 `[closed_tasks]` 段由 LLM 填充,实测曾混入 `[lessons]` 标签行
/// 与教训句式(如"应使用 ...")—— 污染 task_state.closed_tasks,进而使
/// fixed_memory completed 列表冗余且字节抖动。此处剔除:
/// - `[xxx]` 标签行(如 `[lessons]`)
/// - 指令/教训句式(含 "应使用" / "不要 " / "需先",任务完成声明几乎不会
///   以这些指令式措辞出现)
#[must_use]
fn is_task_completion_like(t: &str) -> bool {
    let trimmed = t.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return false;
    }
    !(trimmed.contains("应使用")
        || trimmed.contains("不要 ")
        || trimmed.contains("需先"))
}

/// 截取 `start_marker` 下一行起、`end_marker` 前的文本段。
/// `end_marker` 为空串时取到文本结尾;`end_marker` 不存在时也取到结尾。
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

/// 提取形如 `field: value` 行的值(字段名大小写不敏感,按 ASCII 匹配)。
///
/// 兼容 LLM 摘要路径:`sanitize_llm_summary` 会把输出每一行规范化为
/// `- {line}` bullet,因此真实入参形如 `- goal: X` / `- next_action: Y`。
/// 之前只匹配行首 `goal:`,导致 LLM 路径的 goal/next_action 永远解析失败
/// (2026-08-14 实证:会话摘要含 [active_task] 段但字段始终为空)。
fn strip_field_prefix<'a>(line: &'a str, field: &str) -> Option<String> {
    let trimmed = line.trim();
    let bare = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .unwrap_or(trimmed);
    let lower = bare.to_ascii_lowercase();
    let prefix = format!("{field}:");
    if lower.starts_with(&prefix) {
        Some(bare[field.len() + 1..].trim().to_string())
    } else {
        None
    }
}

/// 判断值是否为"无内容"占位(NONE / None / none / 空)。
fn is_none_value(v: &str) -> bool {
    v.is_empty() || v.eq_ignore_ascii_case("none")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> TaskState {
        TaskState::default()
    }

    #[test]
    fn update_captures_goal_and_findings() {
        let mut state = ts();
        state.update_from_turn(
            "调查 BTC 30分钟笔绘制数量差异:手绘 6 笔 vs 前端 4 笔",
            &[
                "开始调查根因".to_string(),
                "关键发现:strict_fenxing=True 漏检 2 个分型".to_string(),
                "顺便看看文件".to_string(),
            ],
        );
        assert!(state.goal.contains("BTC"));
        assert!(state.findings.len() == 1, "{:?}", state.findings);
        assert!(state.findings[0].contains("strict_fenxing"));
    }

    #[test]
    fn short_input_keeps_existing_goal() {
        let mut state = ts();
        state.update_from_turn("调查 A 问题", &[]);
        let goal_before = state.goal.clone();
        state.update_from_turn("继续", &["根因确认:xxx".to_string()]);
        assert_eq!(state.goal, goal_before, "短输入不应覆盖 goal");
        assert_eq!(state.findings.len(), 1);
    }

    #[test]
    fn new_task_clears_findings() {
        let mut state = ts();
        state.update_from_turn("任务甲:修复模块 A", &["根因:缓存失效".to_string()]);
        assert_eq!(state.findings.len(), 1);
        // 换任务
        state.update_from_turn("任务乙:优化模块 B 性能", &["结论:改用缓存".to_string()]);
        assert_eq!(state.goal, "任务乙:优化模块 B 性能");
        assert!(state.findings.iter().all(|f| !f.contains("缓存失效")));
        assert_eq!(state.findings.len(), 1);
    }

    #[test]
    fn findings_capped_and_deduped() {
        let mut state = ts();
        let mut texts = Vec::new();
        for i in 0..10 {
            texts.push(format!("关键发现 {i}: 结论是数值 {i}"));
        }
        state.update_from_turn("长任务", &texts);
        assert!(state.findings.len() <= TASK_FINDINGS_MAX);
        let mut unique = state.findings.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), state.findings.len(), "findings 不应有重复");
    }

    #[test]
    fn findings_require_conclusion_form() {
        // 防污染回归:命中关键词但无结论形态(无冒号、无强判定标记)的行
        // 是"讨论/猜测",不是"已确认结论",不得进入 findings。
        let mut state = ts();
        state.update_from_turn(
            "排查模块 A 崩溃问题",
            &[
                "根因可能涉及缓存层".to_string(),
                "关键结论:根因是缓存过期导致崩溃".to_string(),
                "测试用例 PASS".to_string(),
            ],
        );
        assert_eq!(state.findings.len(), 2, "{:?}", state.findings);
        assert!(
            !state.findings.iter().any(|f| f.contains("可能涉及")),
            "speculative line must not be a finding: {:?}",
            state.findings
        );
        assert!(state.findings.iter().any(|f| f.contains("缓存过期")));
        assert!(state.findings.iter().any(|f| f.contains("PASS")));
    }

    #[test]
    fn render_empty_is_empty() {
        assert!(ts().render_for_prompt().is_empty());
    }

    #[test]
    fn parse_summary_extracts_active_and_closed_tasks() {
        let summary = "- 修复了登录 401 问题\n\
                       - 关键文件: auth.ts, session.ts\n\
                       \n\
                       [active_task]\n\
                       goal: 兼容旧 Session 格式\n\
                       next_action: 补一个迁移测试\n\
                       \n\
                       [closed_tasks]\n\
                       - 登录 401 修复: 6/6 PASS\n\
                       - auth 重构: 已收尾";
        let extract = parse_task_state_from_summary(summary);
        assert_eq!(extract.active_goal.as_deref(), Some("兼容旧 Session 格式"));
        assert_eq!(extract.next_action.as_deref(), Some("补一个迁移测试"));
        assert_eq!(
            extract.closed_tasks,
            vec![
                "登录 401 修复: 6/6 PASS".to_string(),
                "auth 重构: 已收尾".to_string()
            ]
        );
    }

    #[test]
    fn parse_summary_handles_none_and_missing_sections() {
        // 无进行中任务 → goal/next_action 为 NONE → 不提取
        let none = parse_task_state_from_summary(
            "[active_task]\ngoal: NONE\nnext_action: NONE\n\n[closed_tasks]\nNONE",
        );
        assert_eq!(none.active_goal, None);
        assert_eq!(none.next_action, None);
        assert!(none.closed_tasks.is_empty());

        // 启发式摘要(无结构化段)→ 空提取,调用方跳过
        let heuristic = parse_task_state_from_summary("- 修复了登录问题\n- 关键文件: auth.rs");
        assert_eq!(heuristic, SummaryTaskExtract::default());

        // 大小写不敏感字段名 + 空文本
        let upper = parse_task_state_from_summary(
            "[active_task]\nGOAL: 优化模块 B\nnext_action: 跑基准测试",
        );
        assert_eq!(upper.active_goal.as_deref(), Some("优化模块 B"));
        assert!(parse_task_state_from_summary("").closed_tasks.is_empty());
    }

    #[test]
    fn parse_summary_filters_label_and_lesson_lines_from_closed_tasks() {
        // C3 实证污染:LLM 字段混淆,把 [lessons] 标签与教训句式写进
        // [closed_tasks] 段。应被 is_task_completion_like 全部过滤。
        let summary = "[active_task]\n\
                       goal: 修复模块 A\n\
                       \n\
                       [closed_tasks]\n\
                       - 已修复模块 A 的登录 401。PASS\n\
                       - [lessons]\n\
                       - 使用 `git stash` 做基线对比时进度输出混入 stdout,应使用 `git stash push -q`\n\
                       - 完成提示词质量初步审查(结构化问题清单)PASS";
        let extract = parse_task_state_from_summary(summary);
        assert_eq!(extract.closed_tasks.len(), 2);
        assert!(extract
            .closed_tasks
            .iter()
            .all(|t| !t.starts_with('[') && !t.contains("应使用")));
        assert!(extract
            .closed_tasks
            .iter()
            .any(|t| t.contains("登录 401")));
        assert!(extract
            .closed_tasks
            .iter()
            .any(|t| t.contains("提示词质量初步审查")));
    }

    #[test]
    fn parse_summary_handles_bullet_prefixed_fields() {
        // LLM 摘要路径:sanitize_llm_summary 给每行加 "- " 前缀。
        // 修复前 strip_field_prefix 只匹配行首 "goal:",bullet 前缀
        // 导致 goal/next_action 永远解析为空(2026-08-14 实证)。
        let summary = "- [active_task]\n\
                       - goal: 压缩摘要质量修复\n\
                       - next_action: 重新编译部署并实机验证\n\
                       \n\
                       - [closed_tasks]\n\
                       - workspace_root 注入: 已修复 PASS";
        let extract = parse_task_state_from_summary(summary);
        assert_eq!(extract.active_goal.as_deref(), Some("压缩摘要质量修复"));
        assert_eq!(
            extract.next_action.as_deref(),
            Some("重新编译部署并实机验证")
        );
        assert_eq!(
            extract.closed_tasks,
            vec!["workspace_root 注入: 已修复 PASS".to_string()]
        );
    }

    #[test]
    fn save_and_load_roundtrip() {
        let mut state = ts();
        state.update_from_turn("测试任务", &["根因已确认:网络超时".to_string()]);
        let path = std::env::temp_dir().join(format!(
            "claw-task-state-test-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        state.save(&path).expect("save");
        let loaded = TaskState::load(&path).expect("load");
        assert_eq!(loaded.goal, state.goal);
        assert_eq!(loaded.findings, state.findings);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_completed_subgoals_dedupes_and_caps() {
        let mut state = ts();
        state.record_completed_subgoals(&[
            "step A".to_string(),
            "step A".to_string(), // 重复应去重
            "step B".to_string(),
        ]);
        assert_eq!(state.completed_subgoals, vec!["step A", "step B"]);

        // 上限:追加超过 TASK_FINDINGS_MAX 后,超出部分丢弃
        for i in 0..(TASK_FINDINGS_MAX + 3) {
            state.record_completed_subgoals(&[format!("extra {i}")]);
        }
        assert!(
            state.completed_subgoals.len() <= TASK_FINDINGS_MAX,
            "completed_subgoals should be capped, got {}",
            state.completed_subgoals.len()
        );
    }

    #[test]
    fn completed_subgoals_render_and_roundtrip() {
        let mut state = ts();
        state.goal = "重构 auth 模块".to_string();
        state.record_completed_subgoals(&["拆分 token 校验".to_string()]);
        let rendered = state.render_for_prompt();
        assert!(rendered.contains("已完成的子目标"));
        assert!(rendered.contains("拆分 token 校验"));

        let path = std::env::temp_dir().join(format!(
            "claw-task-state-subgoals-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        state.save(&path).expect("save");
        let loaded = TaskState::load(&path).expect("load");
        assert_eq!(loaded.completed_subgoals, state.completed_subgoals);
        let _ = std::fs::remove_file(&path);
    }
}
