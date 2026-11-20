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
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 任务目标的最大字符数(恒定体积,避免无限增长)。
pub const TASK_GOAL_MAX_CHARS: usize = 150;
/// 单条关键发现的最大字符数。
pub const TASK_FINDING_MAX_CHARS: usize = 120;
/// findings 容量上限(保留最新 N 条)。
pub const TASK_FINDINGS_MAX: usize = 6;
/// task_state 持久化文件名(位于 `.claw/` 下)。
pub const TASK_STATE_FILE: &str = "task_state.json";

/// 会话级 task_state 文件路径:会话目录 + session_id。
///
/// 会话级隔离:goal/findings/completed_subgoals 绑定单一会话,新会话使用
/// 自己的文件,从根上消除"旧任务目标注入新会话"的污染(2026-08-31 实证:
/// 工作区级 task_state 跨会话残留导致 Current Task 注入错误目标)。
/// 会话目录由调用方解析(与 NOTEBOOK / tracker 同构,见
/// `crate::session::workspace_sessions_dir`)。
#[must_use]
pub fn session_task_state_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}-task_state.json"))
}

/// 判定"用户换新任务"的 goal 前缀比较长度。
const GOAL_PREFIX_COMPARE_CHARS: usize = 30;
/// 用户输入短于此字符数时不视为完整任务描述,进入短输入语义判定。
const MIN_GOAL_INPUT_CHARS: usize = 8;

/// 纯承接词 —— 不携带任务语义的短输入,不应覆盖任务锚点。
///
/// v5 迭代修复:原逻辑 `chars >= MIN_GOAL_INPUT_CHARS` 一刀切拦截短输入,
/// 导致短任务名("MVP-C1"=6字符)被丢弃、Current Task 锚点停留旧任务;
/// 同时宽松化后 '好滴'/'没问题'/'abc'/'abcde' 等又会误判为新任务。
/// 这里显式枚举承接词,命中即不更新 goal(大小写不敏感)。
const ACK_WORDS: &[&str] = &[
    "继续",
    "好",
    "好的",
    "ok",
    "嗯",
    "收到",
    "知道了",
    "明白",
    "可以",
    "行",
    "好的好的",
    "好滴",
    "没问题",
    "嗯嗯",
    "很好",
    "了解了",
    "好的吧",
    "okay",
    "okey",
    "了解",
    "晓得了",
    "是",
    "对",
    "不错",
    "继续吧",
    "继续做",
    "继续干",
    "随便",
    "随便吧",
    "无所谓",
    "都行",
    "都行吧",
    "随意",
    "好嘞",
    "好勒",
    "好呀",
    "行吧",
    "好吧",
];

/// 判定用户输入是否应更新任务目标(v5 语义判定,替代原长度一刀切)。
///
/// 第一性原理:任务身份切换的判据是"输入是否携带任务语义",而非"输入长度"。
/// 原逻辑用长度代理语义,导致短任务名被拦截(Current Task 锚点滞后缺陷根因)。
/// v5 规则(原型 29/29 收敛,修复 '好滴'/'没问题'/'abc'/'abcde' 四误判):
/// 1. 纯承接词(继续/好滴/没问题/随便…)→ 不更新(防覆盖锚点)
/// 2. 长输入(≥[`MIN_GOAL_INPUT_CHARS`])→ 完整任务描述,更新
/// 3. 短输入纯 ASCII 小写字母+数字(abc/abcde/123)→ 噪声,不更新
/// 4. 其余短输入(含大写/连字符/中文任务名,如 MVP-C1/方案C/任务3)→ 更新
fn should_update_goal(new_goal: &str) -> bool {
    let trimmed = new_goal.trim();
    if trimmed.is_empty() {
        return false;
    }
    // 1. 纯承接词 → 不更新。
    let lower = trimmed.to_lowercase();
    if ACK_WORDS.iter().any(|w| *w == lower) {
        return false;
    }
    // 2. 长输入 → 更新(完整任务描述)。
    if trimmed.chars().count() >= MIN_GOAL_INPUT_CHARS {
        return true;
    }
    // 3. 短输入纯 ASCII 小写字母+数字 → 噪声,不更新。
    if trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return false;
    }
    // 4. 其余短输入(任务名信号:大写/连字符/中文)→ 更新。
    true
}

/// 外部注入上下文块的起始标记 —— 这些块由外壳层（Web 端
/// `prompt_with_web_context`）前置拼进 user 文本，**不是用户发言**。
///
/// 与 `rusty-claude-cli/src/serve.rs::prompt_with_web_context` 的三类块一一对应；
/// 新增注入块时必须同步此处，否则该块会被当成用户任务描述。
const INJECTED_PREFIX_HEADERS: &[&str] = &["[计划模式]", "[Web 计划", "[Web 待办状态"];

/// 剥离消息开头由外壳层注入的上下文块，返回真实用户发言。
///
/// # 动机
///
/// Web 端把「计划模式指令 / Web 计划 / Web 待办」三类块前置拼进 user 文本再
/// 入库（`prompt_with_web_context`）。规则式 goal 提取拿到的 `user_input`
/// 因此以注入块开头，会被当作任务描述写进会话级 `task_state.goal`，再经
/// Current Task 槽位**每轮注入** —— 一次注入即永久固化，且新会话不复位
/// （2026-11-19 实证：新会话 `session-1795017862696-0` 的 goal 首行即
/// `[Web 计划（用户在 Web 端维护…）]`，导致「新开会话却带着旧任务」）。
///
/// # 规则
///
/// 从首行起，命中 [`INJECTED_PREFIX_HEADERS`] 即吞掉该块直到空行为止（块内
/// 不含空行，与注入端 `\n\n` 分隔一致）；遇到非注入行即停止。全部被吞掉时
/// 返回空串 —— [`should_update_goal`] 对空串返回 false，goal 保持原值。
///
/// 只剥离**开头**的连续块，不做全文替换：用户正文里出现的同形文本（如引用
/// 一段日志）不受影响。
#[must_use]
pub fn strip_injected_prefix(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        let is_injected = INJECTED_PREFIX_HEADERS
            .iter()
            .any(|header| rest.starts_with(header));
        if !is_injected {
            return rest.trim_end();
        }
        // 块以空行结束（注入端用 `\n\n` 分隔块与正文）。
        match rest.find("\n\n") {
            Some(index) => rest = rest[index + 2..].trim_start(),
            None => return "",
        }
    }
}

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
///
/// 注意这里**不得出现裸 `结论`**:它会把"## 结论""本节结论待定"这类结构性
/// 行/未定陈述一并放行。只收"结论是/结论为"这种主谓完整的判断句。
const FINDING_STRONG_MARKERS: &[&str] = &[
    "根因是",
    "原因是",
    "确认",
    "已验证",
    "结论是",
    "结论为",
    "已修复",
    "修复了",
    "PASS",
    "FAIL",
    "found that",
    "verified",
];

/// 结构化标记行判定:Markdown 标题/表格行/引用块/代码围栏/分隔线。
///
/// 这类行是**排版结构**而非结论陈述。它们命中关键词纯属偶然——`## 结论`
/// 命中"结论"、`| 文献 | 结论 | 影响 |` 命中"结论"——若放进 findings,
/// 会让"已完成项"槽位被表格表头与章节标题占满,把真正的结论挤掉
/// (实测:简报里出现过 `| 文献 | 结论 | 对本实现的影响 |` 这类条目)。
fn is_structural_markup(line: &str) -> bool {
    let raw = line.trim();
    if raw.is_empty() {
        return false;
    }
    // 分隔线 `---` / `===` / `|---|---|` / `***`:只由标点组成。
    //
    // 必须在剥离项目符号**之前**判定:下面 `trim_start_matches('-')` 会把
    // `---` 整行剥成空串,而空串会被后续的 `!trimmed.is_empty()` 短路放行,
    // 导致分隔线漏判(2026-11-19 实测:`---` 与 `|---|---|` 混入 findings)。
    if raw
        .chars()
        .all(|c| matches!(c, '-' | '=' | '|' | ':' | '*' | '_' | '+' | ' ' | '\t'))
    {
        return true;
    }
    let trimmed = raw.trim_start_matches(['·', '-', '*', '+', ' ']);
    if trimmed.starts_with('#')
        || trimmed.starts_with('|')
        || trimmed.starts_with('>')
        || trimmed.starts_with("```")
        || trimmed.starts_with("~~~")
    {
        return true;
    }
    false
}

/// 条目级噪声判定 —— 已落盘条目是否该在加载时剔除。
///
/// 与 [`is_structural_markup`] 的区别:后者只认 Markdown 结构前缀(以 `#`/`|`
/// 开头等),漏掉了**纯标签行** —— 以冒号结尾、冒号后没有任何陈述的行,如
/// `conclusions:`、`## 结论:`、`**修复**(最小改动,两文件):`。这类行是章节
/// 标题或"即将展开"的引子,不是结论本身,却因含关键词/含冒号而通过了提取
/// 门槛(2026-11-19 实测:122 个会话状态文件中 13 条结构性行 + 25 条纯标签行
/// 占据了「已完成项」槽位,把真正的结论挤了出去)。
///
/// 判定顺序:空 → 结构前缀 → 纯标签(以冒号结尾且无后续内容)。
fn is_noise_entry(entry: &str) -> bool {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return true;
    }
    if is_structural_markup(trimmed) {
        return true;
    }
    // 纯标签行:以半角/全角冒号结尾 ⇒ 冒号后无陈述内容。
    // 注意不能放宽成"含冒号" —— `根因:缓存失效` 正是要保留的结论形态。
    trimmed.ends_with(':') || trimmed.ends_with('：')
}

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
    ///
    /// 加载时执行 [`sanitize`](Self::sanitize) —— **存量污染自愈**。提取端的
    /// 修复(见 [`is_noise_entry`])只能阻断新增污染,已落盘的噪声条目会随
    /// 会话 resume 从磁盘原样复活,并经 Current Task 槽位每轮注入。改磁盘
    /// 文件不可行(进程退出时会被内存态覆写),因此在读取路径上过滤。
    #[must_use]
    pub fn load(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        let mut state: Self = serde_json::from_str(&content).ok()?;
        state.sanitize();
        Some(state)
    }

    /// 剔除三个条目列表中的噪声条目(结构性标记行 / 纯标签行)。
    ///
    /// 幂等:对已净化状态再调用不产生变化。不清空 `updated_at_ms` ——
    /// 净化是读取期视图修正,不代表任务状态本身有新进展。
    pub fn sanitize(&mut self) {
        let is_clean = |items: &mut Vec<String>| {
            items.retain(|item| !is_noise_entry(item));
        };
        is_clean(&mut self.findings);
        is_clean(&mut self.completed_subgoals);
        is_clean(&mut self.closed_tasks);
        // goal 同样可能被注入块或标题行污染(实测:`[Web 计划…]` 首行)。
        // 剥离后为空说明原值是纯注入块 —— 此时清空,避免空目标经
        // `render_for_prompt` 产生 "- 目标: " 空壳行。
        let stripped = strip_injected_prefix(&self.goal).trim().to_string();
        if stripped.is_empty() || is_noise_entry(&stripped) {
            self.goal.clear();
        } else if stripped != self.goal {
            self.goal = truncate(&stripped, TASK_GOAL_MAX_CHARS);
        }
    }

    /// 持久化到磁盘。失败返回错误信息(不 panic,不阻断主流程)。
    ///
    /// 写盘前先 [`sanitize`](Self::sanitize) —— **写路径自愈**。只读路径的
    /// 净化(`load`)挡不住这一条:`conversation.rs` 更新任务状态时优先用
    /// 内存态 `self.task_state.clone()`,不经过 `load`,内存里的存量噪声
    /// 会被原样序列化回磁盘,形成"净化→复写→再污染"的循环。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut clean = self.clone();
        clean.sanitize();
        let content =
            serde_json::to_string_pretty(&clean).map_err(|e| format!("serialize: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        std::fs::write(path, content).map_err(|e| format!("write: {e}"))
    }

    /// 从本 turn 规则式更新任务状态(零 LLM 成本)。
    ///
    /// - `user_input`:本 turn 的用户消息。经 [`should_update_goal`] 语义判定
    ///   (承接词/噪声不更新,短任务名/长描述更新);与旧 goal 前缀不同则判定
    ///   为换任务,清空旧 findings。
    /// - `assistant_texts`:本 turn 的 assistant 文本块。逐行扫描含
    ///   [`FINDING_KEYWORDS`] 信号词的行,去重后追加为新发现(保留最新 N 条)。
    pub fn update_from_turn(&mut self, user_input: &str, assistant_texts: &[String]) {
        // 外壳层注入的上下文块不是用户发言，先剥离再判定，否则注入块会被
        // 固化成 goal 并经 Current Task 槽位每轮注入（见 strip_injected_prefix）。
        let trimmed = strip_injected_prefix(user_input).trim();
        let goal = truncate(trimmed, TASK_GOAL_MAX_CHARS);
        if should_update_goal(&goal) {
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
                // 排版结构行不是结论陈述（见 `is_structural_markup`）。
                if is_structural_markup(line) {
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

    /// 记录已收尾任务（`closed_tasks`），防止压缩后指令残留把 AI 拉回已完成任务。
    ///
    /// 写入时机：plan Review 阶段全部 step 成功时由
    /// `sync_completed_subgoals_from_plan` 调用；以及压缩摘要 `[closed_tasks]`
    /// 段解析（见 [`parse_task_state_from_summary`]）。
    pub fn record_closed_tasks(&mut self, tasks: &[String]) {
        for task in tasks {
            if self.closed_tasks.len() >= TASK_FINDINGS_MAX {
                break;
            }
            let task = truncate(task.trim(), TASK_FINDING_MAX_CHARS);
            if task.is_empty() || self.closed_tasks.contains(&task) {
                continue;
            }
            self.closed_tasks.push(task);
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

/// 截断任务目标到 [`TASK_GOAL_MAX_CHARS`](公共包装,供 `set_current_task` 使用)。
#[must_use]
pub fn truncate_goal(s: &str) -> String {
    truncate(s, TASK_GOAL_MAX_CHARS)
}

/// 当前时间戳(ms,公共包装,供 `set_current_task` 持久化使用)。
#[must_use]
pub fn now_ms_pub() -> i64 {
    now_ms()
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

    // R6:优先解析单行 `MEMORY_JSON:`（压缩链三层都会保留该行）；字段缺失时
    // 再回退到人读段解析。
    if let Some(doc) = parse_memory_json(summary) {
        if let Some(goal) = doc
            .get("active_task")
            .or_else(|| doc.get("goal"))
            .and_then(serde_json::Value::as_str)
        {
            if !is_none_value(goal) {
                out.active_goal = Some(truncate(goal, TASK_GOAL_MAX_CHARS));
            }
        }
        if let Some(next) = doc.get("next_action").and_then(serde_json::Value::as_str) {
            if !is_none_value(next) {
                out.next_action = Some(truncate(next, TASK_FINDING_MAX_CHARS));
            }
        }
        if let Some(items) = doc
            .get("closed_tasks")
            .and_then(serde_json::Value::as_array)
        {
            for item in items.iter().filter_map(serde_json::Value::as_str) {
                let item = item.trim();
                if item.is_empty() || is_none_value(item) {
                    continue;
                }
                let t = truncate(item, TASK_FINDING_MAX_CHARS);
                if !out.closed_tasks.contains(&t) {
                    out.closed_tasks.push(t);
                }
                if out.closed_tasks.len() >= TASK_FINDINGS_MAX {
                    break;
                }
            }
        }
    }

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
    !(trimmed.contains("应使用") || trimmed.contains("不要 ") || trimmed.contains("需先"))
}

/// 解析摘要中的单行 `MEMORY_JSON:` 尾行（R6 兼容解析）。
fn parse_memory_json(summary: &str) -> Option<serde_json::Value> {
    let line = summary
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("MEMORY_JSON:"))?;
    let payload = line.trim_start_matches("MEMORY_JSON:").trim();
    serde_json::from_str(payload).ok()
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
fn strip_field_prefix(line: &str, field: &str) -> Option<String> {
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
    fn strip_injected_prefix_removes_web_plan_block() {
        // 现场复现:新会话首条 user 文本 = 注入块 + 用户真实提问。
        let input = "[Web 计划（用户在 Web 端维护，作为当前执行计划）]\n\
                     任务: 第一性原理回答 5 项\n\
                     1. [done] 冒烟\n\n\
                     你好，我想了解一下记忆机制";
        assert_eq!(strip_injected_prefix(input), "你好，我想了解一下记忆机制");
    }

    #[test]
    fn strip_injected_prefix_handles_all_three_blocks() {
        let input = "[计划模式] 当前会话处于计划模式：先用计划形式回应。\n\n\
                     [Web 待办状态（用户在 Web 端维护，可作为当前任务清单）]\n\
                     - [ ] 写测试\n\n\
                     真实提问";
        assert_eq!(strip_injected_prefix(input), "真实提问");
    }

    #[test]
    fn strip_injected_prefix_keeps_plain_user_text() {
        // 无注入块时原样返回（仅去首尾空白）。
        assert_eq!(strip_injected_prefix("  修复缓存缺陷  "), "修复缓存缺陷");
        // 正文中出现的同形文本不受影响（只剥离开头连续块）。
        let mid = "看这段日志\n[Web 计划] 是注入的";
        assert_eq!(strip_injected_prefix(mid), mid);
    }

    #[test]
    fn injected_prefix_does_not_pollute_goal() {
        // 端到端:注入块不得进入 goal，且原有 goal 不被覆盖。
        let mut state = ts();
        state.update_from_turn("调查缓存命中率下降", &[]);
        let goal_before = state.goal.clone();
        let injected = "[Web 计划（用户在 Web 端维护，作为当前执行计划）]\n\
                        任务: 实施 5 项\n\n";
        state.update_from_turn(injected, &[]);
        assert_eq!(state.goal, goal_before, "纯注入块不应改动 goal");

        // 注入块 + 真实提问 → goal 取真实提问部分。
        let injected_with_text = "[Web 计划（用户在 Web 端维护，作为当前执行计划）]\n\
                                  任务: 实施 5 项\n\n\
                                  帮我检查记忆机制是否完善";
        state.update_from_turn(injected_with_text, &[]);
        assert!(
            !state.goal.contains("Web 计划"),
            "goal 不应含注入块: {}",
            state.goal
        );
        assert!(state.goal.contains("记忆机制"), "{}", state.goal);
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
    fn short_task_name_updates_goal() {
        // v5 修复:短任务名("MVP-C1" 6 字符)应更新 goal,不再被 8 字符门槛拦截。
        let mut state = ts();
        state.update_from_turn("先把 MVP-C2 落地", &[]);
        assert_eq!(state.goal, "先把 MVP-C2 落地");
        state.update_from_turn("MVP-C1", &[]);
        assert_eq!(state.goal, "MVP-C1", "短任务名应切换 goal");
    }

    #[test]
    fn ack_words_do_not_update_goal() {
        // v5 修复:承接词(继续/好滴/没问题/随便…)不更新 goal(防覆盖锚点)。
        let mut state = ts();
        state.update_from_turn("调查根因:缓存失效", &[]);
        let goal_before = state.goal.clone();
        for ack in ["继续", "好滴", "没问题", "随便", "好的", "OK", "收到"] {
            state.update_from_turn(ack, &[]);
            assert_eq!(state.goal, goal_before, "承接词 '{ack}' 不应覆盖 goal");
        }
    }

    #[test]
    fn noise_words_do_not_update_goal() {
        // v5 修复:纯小写字母数字噪声(abc/abcde/123)不更新 goal。
        let mut state = ts();
        state.update_from_turn("修复决策日志去重", &[]);
        let goal_before = state.goal.clone();
        for noise in ["abc", "abcde", "123", "xyz", "abc123"] {
            state.update_from_turn(noise, &[]);
            assert_eq!(state.goal, goal_before, "噪声 '{noise}' 不应覆盖 goal");
        }
    }

    #[test]
    fn should_update_goal_covers_all_v5_cases() {
        // 直接验证 should_update_goal 的完整判定面。
        // 更新:
        for input in [
            "MVP-C1",
            "方案C",
            "任务3",
            "先把 MVP-C2 落地",
            "修复 task_state 缺陷",
        ] {
            assert!(should_update_goal(input), "'{input}' 应更新 goal");
        }
        // 不更新(承接词):
        for input in ["继续", "好滴", "没问题", "随便", "好的", "OK", "嗯", "收到"] {
            assert!(!should_update_goal(input), "'{input}' 不应更新 goal");
        }
        // 不更新(噪声):
        for input in ["abc", "abcde", "123", "xyz"] {
            assert!(!should_update_goal(input), "'{input}' 不应更新 goal");
        }
        // 空输入:
        assert!(!should_update_goal(""));
        assert!(!should_update_goal("   "));
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
    fn structural_markup_never_becomes_finding() {
        // 实测污染:固定记忆的「已完成项」被 Markdown 表格表头与章节标题占满
        // (`| 文献 | 结论 | 对本实现的影响 |`、`## 结论`)。它们是排版结构,
        // 命中"结论"纯属偶然,进入 findings 会把真结论挤掉。
        let mut state = ts();
        state.update_from_turn(
            "排查渲染层污染",
            &[
                "| 文献 | 结论 | 对本实现的影响 |".to_string(),
                "|---|---|---|".to_string(),
                "## 结论".to_string(),
                "### 三、遗留污染清理".to_string(),
                "> 结论:引用块不是结论陈述".to_string(),
                "- 结论:列表项里的才是真结论".to_string(),
            ],
        );
        assert!(
            !state
                .findings
                .iter()
                .any(|f| f.contains('|') || f.starts_with('#') || f.starts_with('>')),
            "结构性行不得进入 findings: {:?}",
            state.findings
        );
        assert!(
            state.findings.iter().any(|f| f.contains("列表项里")),
            "真正的列表结论应保留: {:?}",
            state.findings
        );
    }

    #[test]
    fn bare_conclusion_marker_does_not_admit_headings() {
        // `FINDING_STRONG_MARKERS` 曾含裸 "结论",使任何含该词的行(含
        // `## 结论`)都过 is_conclusion。改为"结论是/结论为"后,未定陈述
        // 与结构性行都挡得住,而完整判断句仍进得来。
        let mut state = ts();
        state.update_from_turn(
            "验证标记收紧",
            &[
                "结论待定".to_string(),
                "本节结论将在下轮补充".to_string(),
                "结论是缓存层失效".to_string(),
            ],
        );
        assert!(
            !state.findings.iter().any(|f| f.contains("待定")),
            "未定陈述不得进入 findings: {:?}",
            state.findings
        );
        assert!(
            state.findings.iter().any(|f| f.contains("缓存层失效")),
            "完整判断句应保留: {:?}",
            state.findings
        );
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
        assert!(extract.closed_tasks.iter().any(|t| t.contains("登录 401")));
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

    #[test]
    fn is_noise_entry_rejects_markup_and_label_only_lines() {
        // 结构性行:标题 / 表格 / 引用 / 围栏 / 分隔线
        for line in [
            "## 结论",
            "### 复核结论",
            "| 文献 | 结论 | 对本实现的影响 |",
            "| Q | 结论 | 提交 | 证据 |",
            "> 引用块里的结论",
            "```",
            "|---|---|",
            "---",
        ] {
            assert!(is_noise_entry(line), "should be noise: {line}");
        }
        // 纯标签行:冒号后无陈述(半角/全角)
        for line in ["conclusions:", "**修复**(最小改动,两文件):", "本轮收尾："] {
            assert!(is_noise_entry(line), "should be noise: {line}");
        }
        // 真正的结论形态必须保留 —— 含冒号但有陈述内容
        for line in [
            "根因:缓存失效",
            "根因是缓存失效",
            "测试 PASS",
            "修复了注入块未剥离的问题",
        ] {
            assert!(!is_noise_entry(line), "should be kept: {line}");
        }
    }

    #[test]
    fn load_sanitizes_legacy_polluted_entries() {
        // 模拟磁盘上的存量污染:渲染层此前把标题/表格行写进了三个列表。
        let path = std::env::temp_dir().join(format!(
            "claw-task-state-sanitize-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        let polluted = serde_json::json!({
            "goal": "[Web 计划（用户在 Web 端维护，作为当前执行计划）]\n任务: 旧任务\n\n真实目标",
            "findings": [
                "## 结论:机制没有被破坏",
                "| 文献 | 结论 | 对本实现的影响 |",
                "conclusions:",
                "根因:缓存失效",          // 应保留
            ],
            "completed_subgoals": ["## 已保存（本轮实测确认）", "拆分 token 校验"],
            "closed_tasks": ["Newly compacted context:", "旧任务已完成"],
            "updated_at_ms": 1_700_000_000_000i64,
        });
        std::fs::write(&path, serde_json::to_string(&polluted).unwrap()).expect("write");

        let loaded = TaskState::load(&path).expect("load");
        assert_eq!(loaded.findings, vec!["根因:缓存失效".to_string()]);
        assert_eq!(
            loaded.completed_subgoals,
            vec!["拆分 token 校验".to_string()]
        );
        assert_eq!(loaded.closed_tasks, vec!["旧任务已完成".to_string()]);
        // goal 剥离注入块后保留真实部分,而非整体清空。
        assert!(!loaded.goal.starts_with("[Web 计划"));
        assert!(loaded.goal.contains("真实目标"));

        // 幂等:再次净化不改变结果。
        let mut again = loaded.clone();
        again.sanitize();
        assert_eq!(again, loaded);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sanitize_clears_goal_that_is_pure_injected_block() {
        let mut state = ts();
        state.goal =
            "[Web 待办状态（用户在 Web 端维护，可作为当前任务清单）]\n- [ ] 某待办".to_string();
        state.sanitize();
        assert!(
            state.goal.is_empty(),
            "纯注入块应清空 goal, got {:?}",
            state.goal
        );
    }
}
