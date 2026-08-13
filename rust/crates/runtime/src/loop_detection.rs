//! LoopDetectionMiddleware — Step 2.2 打断 Doom Loop(同文件 10+ 次编辑)。
//!
//! 设计文档:`docs/harness-engineering-optimization-plan.md` Step 2.2
//!
//! 架构:
//! - [`LoopDetector`][]:跟踪每个文件的编辑次数,在阈值处触发注入上下文或中止。
//! - [`LoopAction`]:中间件输出 — Continue / InjectContext / Abort。
//! - Abort 行为(经分析修订):**不**走 RecoveryOrchestrator —— 恢复编排器面向
//!   worker-boot(trust prompt / MCP handshake / 编译修复),与主 agent 循环场景
//!   不匹配,且默认模拟 executor 会把恢复误报为成功、多跑一轮 doomed 迭代。
//!   实际由 conversation 工具循环检测到 `loop_abort_reason` 后**直接终止 turn**,
//!   诊断信息写入 NOTEBOOK `<attempted>` 段供下一轮改变策略。
//!
//! 阈值:
//! - 5 次同文件编辑 → `InjectContext("consider reconsidering your approach")`
//! - 10 次同文件编辑 → `Abort("doom loop detected")`
//! - 3 次相同 (tool_name, 规范化 input) 调用 → `InjectContext`
//! - 6 次相同 (tool_name, 规范化 input) 调用 → `Abort`
//! - 5 次相同 (tool_name, 规范化 output) 调用 → `InjectContext`
//! - 10 次相同 (tool_name, 规范化 output) 调用 → `Abort`
//! - 15 次连续无产出(非文件修改)工具调用 → `InjectContext`(探索循环)
//! - 30 次连续无产出工具调用 → `Abort`
//!
//! 经验法则:
//! - MCP Tools ≤ 80, Skills ≤ 15,注册时校验。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const WARN_THRESHOLD: u32 = 5;

/// 同文件编辑触发中止的阈值(Doom Loop 检测)。
pub const ABORT_THRESHOLD: u32 = 10;

/// 相同 (tool_name, 规范化 input) 调用触发警告的阈值。
///
/// 针对 bash 诊断循环(如反复执行 `netstat`/`tail` 验证同一状态):
/// 完全相同的命令重复 3 次即提示重新考虑方向。
pub const TOOL_WARN_THRESHOLD: u32 = 3;

/// 相同 (tool_name, 规范化 input) 调用触发中止的阈值。
pub const TOOL_ABORT_THRESHOLD: u32 = 6;

/// 相同 (tool_name, output) 调用触发警告的阈值(输入可能不同,但结果无变化,
/// 例如 `sleep 3 && tail` / `sleep 5 && tail` 输出相同 —— 典型的验证循环)。
pub const SAME_OUTPUT_WARN_THRESHOLD: u32 = 5;

/// 相同 (tool_name, output) 调用触发中止的阈值。
pub const SAME_OUTPUT_ABORT_THRESHOLD: u32 = 10;

/// 连续无产出(非文件修改)工具调用触发警告的阈值。
///
/// 针对"探索循环":每次调用不同命令/工具(相同-input 与相同-output 通道
/// 均无法命中),但从不修改任何文件、任务无进展。达到该次数注入提示,
/// 引导模型改变策略或询问用户。
pub const NO_OUTPUT_WARN_THRESHOLD: u32 = 15;

/// 连续无产出工具调用触发中止的阈值。
///
/// 高于警告阈值:给"边探索边思考"的复杂分析留足空间(文本输出/文件修改
/// 都会清零 streak),同时保留硬终止兜底,防止模型无视警告后无限空转。
pub const NO_OUTPUT_ABORT_THRESHOLD: u32 = 30;

/// MCP Tools 注册数量上限(经验法则)。
pub const MCP_TOOLS_MAX: usize = 80;

/// Skills 注册数量上限(经验法则)。
pub const SKILLS_MAX: usize = 15;

/// 认知停滞(连续纠结思考)触发溯源提示的阈值。
///
/// 连续 [`COG_STALL_WARN_THRESHOLD`] 轮 thinking 命中
/// [`COG_STALL_MARKERS`] 纠结标记 → 注入"不确定性溯源"提示。
/// 取值 5:正常深度思考中纠结标记稀疏(偶发 1 次),连续 5 轮命中是
/// "在同一个认知分叉点上空转"的强信号(实证:AI 会连续 20+ 轮纠结)。
pub const COG_STALL_WARN_THRESHOLD: u32 = 5;

/// 纠结/自我否定标记词(从真实会话 thinking 中提取的高频信号)。
///
/// 命中任一即视为一轮"停滞思考"。选择聚焦在"不确定性/否定/反复"的强信号,
/// 排除"等等/不对/嗯"等过于常见、易误伤正常思考的弱词。
const COG_STALL_MARKERS: &[&str] = &[
    "不能猜", "不能凭空", "无法确定", "难以确定", "需要确认",
    "我不确定", "不确定", "让我再想", "让我重新想", "纠结", "矛盾",
];

/// 认知停滞时注入的"不确定性溯源"提示(四分支,与领域无关)。
///
/// 引导模型判断"反复纠结的问题,答案在哪个信息源",而不是继续用推理
/// 猜测一个只有外部(用户/代码)才能回答的事实性问题。
const COG_STALL_TRACE_PROMPT: &str = "\
你在最近连续多轮思考中反复出现「不确定 / 需要确认 / 纠结」等信号,但尚未收敛。请立即执行一次「不确定性溯源」,不要继续猜测:

1. 答案是【只有用户才知道】的吗?→ 立即停止推理,向用户提问澄清(明确列出你无法确定的具体点)。
2. 答案是【代码 / 文档里才能查到】的吗?→ 做一次精确检索定位;若已多次检索仍未收敛,说明答案可能不在此源,回到第 1 条。
3. 答案是【逻辑上可推出】的吗?→ 明确写下推理,得出唯一结论,停止反复。
4. 答案是【任何渠道都无法确定】的吗?→ 显式标注为「假设」,带着假设继续推进,不要原地打转。";

/// 第 2 层(事后沉淀):认知停滞触发的通用失败模式教训。
///
/// 与 [`COG_STALL_TRACE_PROMPT`](第 1 层事中兜底) 不同,这是**已发生的失败事件**
/// 的证据,沉淀为跨会话教训(经 `lessons` 机制注入),把第 0 层的"理论准则"
/// 升级为"实证警告"。内容保持通用(不绑定具体领域术语),可跨任务复用。
pub const COG_STALL_LESSON: &str = "猜测循环:连续多轮思考反复纠结同一不确定点,用推理猜测只有外部(用户/代码)才能回答的事实问题,浪费 token 未收敛。陷入纠结时先溯源信息源(问用户/查代码/逻辑推导/标假设),不要继续猜测。";

/// 中间件输出 — LoopDetector 每次记录编辑后返回的动作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopAction {
    /// 正常继续,无需干预。
    Continue,
    /// 编辑次数达到警告阈值,注入上下文建议重新考虑方法。
    InjectContext(String),
    /// 编辑次数达到中止阈值,检测到 Doom Loop,必须中止当前 turn。
    Abort(String),
}

/// 跟踪每个文件的编辑次数,在阈值处触发干预。
///
/// # Example
/// ```
/// use runtime::loop_detection::{LoopDetector, LoopAction, WARN_THRESHOLD, ABORT_THRESHOLD};
///
/// let mut detector = LoopDetector::new();
/// // 低于 WARN_THRESHOLD → Continue
/// for _ in 0..WARN_THRESHOLD - 1 {
///     assert!(matches!(detector.record_edit("src/main.rs"), LoopAction::Continue));
/// }
/// // 达到 WARN_THRESHOLD → InjectContext
/// assert!(matches!(detector.record_edit("src/main.rs"), LoopAction::InjectContext(_)));
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopDetector {
    /// 每个文件路径的编辑计数。
    edit_counts: HashMap<String, u32>,
    /// 累计总编辑次数(跨所有文件)。
    total_edits: u64,
    /// 是否已经对该文件发出过警告(避免重复注入)。
    warned: HashMap<String, bool>,
    /// (tool_name, 规范化 input) → (调用次数, 最后调用时间戳 ms)。
    /// 时间戳用于跨 turn 衰减:窗口内跨 turn 累积,超时自动清零。
    tool_call_counts: HashMap<(String, String), (u32, u64)>,
    /// (tool_name, 规范化 output) → (出现次数, 最后出现时间戳 ms)。
    tool_output_counts: HashMap<(String, String), (u32, u64)>,
    /// 已发出过警告的调用 key → 警告时间戳 ms。衰减时一并清除,
    /// 允许窗口期过后对新一轮循环重新警告。
    tool_warned: HashMap<String, u64>,
    /// 连续非文件修改工具调用计数(探索循环检测)。
    /// 命中文件修改工具时清零;达到 [`NO_OUTPUT_WARN_THRESHOLD`] /
    /// [`NO_OUTPUT_ABORT_THRESHOLD`] 时警告/中止。每 turn 由
    /// [`LoopDetector::reset_edits`] 重置:跨 turn 的"连续无产出"语义不成立
    /// (turn 之间有用户输入与模型重新评估),跨 turn 相同调用循环已由
    /// [`LoopDetector::prune_decayed`] 覆盖的工具调用计数通道检测。
    no_output_run: u32,
    /// 是否已对当前无产出 streak 发出过警告(避免重复注入)。
    no_output_warned: bool,
    /// 连续命中纠结标记的 thinking 轮数(认知停滞累计)。
    /// 命中 [`COG_STALL_MARKERS`] 累计,不命中清零;达到
    /// [`COG_STALL_WARN_THRESHOLD`] 时注入溯源提示。每 turn 由
    /// [`LoopDetector::reset_edits`] 重置(认知停滞是单 turn 内现象)。
    cog_stall_run: u32,
    /// 是否已对当前认知停滞发出过溯源提示(避免重复注入)。
    cog_stall_warned: bool,
}

impl LoopDetector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次文件编辑,返回应采取的动作。
    ///
    /// - 低于 `WARN_THRESHOLD` → `Continue`
    /// - 等于 `WARN_THRESHOLD` → `InjectContext("consider reconsidering your approach")`
    /// - 大于 `WARN_THRESHOLD` 且低于 `ABORT_THRESHOLD` → `Continue`(已警告)
    /// - 等于或超过 `ABORT_THRESHOLD` → `Abort("doom loop detected: <file> edited <n> times")`
    #[must_use]
    pub fn record_edit(&mut self, file_path: &str) -> LoopAction {
        let count = self.edit_counts.entry(file_path.to_owned()).or_insert(0);
        *count += 1;
        self.total_edits += 1;
        // 文件编辑是任务产出信号:清零无产出 streak。
        self.no_output_run = 0;
        self.no_output_warned = false;

        if *count >= ABORT_THRESHOLD {
            LoopAction::Abort(format!(
                "doom loop detected: {file_path} edited {count} times"
            ))
        } else if *count == WARN_THRESHOLD && !self.warned.get(file_path).copied().unwrap_or(false)
        {
            self.warned.insert(file_path.to_owned(), true);
            LoopAction::InjectContext(
                "consider reconsidering your approach — this file has been edited many times"
                    .to_owned(),
            )
        } else {
            LoopAction::Continue
        }
    }

    /// 记录一次工具调用,检测诊断/验证循环。对所有工具生效(不只文件编辑)。
    ///
    /// 两个信号:
    /// - **完全相同调用**:`(tool_name, 规范化 input)` 相同。阈值
    ///   [`TOOL_WARN_THRESHOLD`] / [`TOOL_ABORT_THRESHOLD`](3/6)。
    /// - **输出无变化**:`(tool_name, 规范化 output)` 相同(输入可能不同,
    ///   如 `sleep 3 && tail` 与 `sleep 5 && tail` 输出相同)。输出先经
    ///   [`normalize_output`] 剥离时间戳/折叠空白,使带时间戳的日志输出
    ///   也能命中。阈值 [`SAME_OUTPUT_WARN_THRESHOLD`] /
    ///   [`SAME_OUTPUT_ABORT_THRESHOLD`](5/10)。
    ///
    /// 计数按时间戳保留,跨 turn 有效(由 [`LoopDetector::prune_decayed`]
    /// 衰减);优先返回完全相同调用的动作。
    #[must_use]
    pub fn record_tool_call(&mut self, tool_name: &str, tool_input: &str, output: &str) -> LoopAction {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.record_tool_call_at(tool_name, tool_input, output, now_ms)
    }

    /// [`record_tool_call`] 的时间戳注入版本(可测试)。
    fn record_tool_call_at(
        &mut self,
        tool_name: &str,
        tool_input: &str,
        output: &str,
        now_ms: u64,
    ) -> LoopAction {
        // 无产出通道更新(探索循环检测):
        // - 文件修改工具 → 产出信号,清零 streak;
        // - 其他工具 → 累加。置前更新以便下文 Abort 优先级判断使用。
        if crate::slop_scanner::is_file_modifying_tool(tool_name) {
            self.no_output_run = 0;
            self.no_output_warned = false;
        } else {
            self.no_output_run += 1;
        }

        let normalized = normalize_tool_input(tool_input);
        let call_key = (tool_name.to_owned(), normalized);
        let entry = self.tool_call_counts.entry(call_key.clone()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 = now_ms;

        if entry.0 >= TOOL_ABORT_THRESHOLD {
            return LoopAction::Abort(format!(
                "doom loop detected: tool '{tool_name}' invoked {} times with identical input",
                entry.0
            ));
        }
        let mut action = LoopAction::Continue;
        if entry.0 == TOOL_WARN_THRESHOLD {
            let warn_key = format!("call:{tool_name}:{}", call_key.1);
            if let std::collections::hash_map::Entry::Vacant(e) = self.tool_warned.entry(warn_key)
            {
                e.insert(now_ms);
                action = LoopAction::InjectContext(format!(
                    "consider reconsidering your approach — tool '{tool_name}' has been invoked \
                     {} times with identical input; the result has not changed",
                    entry.0
                ));
            }
        }

        // 输出无变化信号(输入不同但结果相同);输出先规范化(剥离时间戳等易变部分)
        let normalized_output = normalize_output(output);
        let out_key = (tool_name.to_owned(), normalized_output);
        let out_entry = self.tool_output_counts.entry(out_key.clone()).or_insert((0, 0));
        out_entry.0 += 1;
        out_entry.1 = now_ms;
        if out_entry.0 >= SAME_OUTPUT_ABORT_THRESHOLD {
            return LoopAction::Abort(format!(
                "doom loop detected: tool '{tool_name}' returned identical output {} times",
                out_entry.0
            ));
        }
        if out_entry.0 == SAME_OUTPUT_WARN_THRESHOLD && matches!(action, LoopAction::Continue) {
            let warn_key = format!("out:{tool_name}:{}", out_key.1);
            if let std::collections::hash_map::Entry::Vacant(e) = self.tool_warned.entry(warn_key)
            {
                e.insert(now_ms);
                action = LoopAction::InjectContext(format!(
                    "consider reconsidering your approach — tool '{tool_name}' returned identical \
                     output {} times; the result has not changed, consider changing strategy \
                     or asking the user",
                    out_entry.0
                ));
            }
        }

        // 无产出通道:连续非文件修改工具调用达到中止阈值 → 终止 turn。
        // 置于相同-input / 相同-output Abort 之后,保证后者优先级更高。
        if self.no_output_run >= NO_OUTPUT_ABORT_THRESHOLD {
            return LoopAction::Abort(format!(
                "no-output loop detected: {0} consecutive tool calls without any file \
                 modification; the task is not progressing. Failed attempts are recorded \
                 in the NOTEBOOK <attempted> section; change strategy or ask the user \
                 before retrying",
                self.no_output_run
            ));
        }
        // 达到警告阈值且未警告过 → 注入"无产出,改变策略"提示。
        if self.no_output_run == NO_OUTPUT_WARN_THRESHOLD && !self.no_output_warned {
            self.no_output_warned = true;
            let msg = format!(
                "consider reconsidering your approach — {0} consecutive tool calls have \
                 produced no file modification; the task is not progressing. Consider \
                 changing strategy or asking the user for direction",
                self.no_output_run
            );
            action = match action {
                LoopAction::InjectContext(existing) => LoopAction::InjectContext(format!(
                    "{existing}\n{msg}"
                )),
                _ => LoopAction::InjectContext(msg),
            };
        }
        action
    }

    /// 记录一次模型文本输出(弱产出信号)。
    ///
    /// 模型输出可见文本(说明分析进度/阶段性结论)时,视为仍在推进思考,
    /// 重置无产出 streak。避免"边说明边探索"的合理诊断(如复杂根因分析)
    /// 被"连续无产出"通道误判为探索循环。文件修改是强产出信号
    /// ([`LoopDetector::record_edit`] 与 `record_tool_call` 中的
    /// `is_file_modifying_tool` 分支),文本输出是弱信号,两者任一出现都清零。
    pub fn record_text_output(&mut self) {
        self.no_output_run = 0;
        self.no_output_warned = false;
    }

    /// 记录一轮 thinking,检测认知停滞(探索/纠结循环的认知维度)。
    ///
    /// thinking 命中任一 [`COG_STALL_MARKERS`] → 累计停滞轮数;否则清零
    /// (本轮是推进性思考)。连续 [`COG_STALL_WARN_THRESHOLD`] 轮命中 →
    /// 返回 [`LoopAction::InjectContext`] 注入"不确定性溯源"提示。
    ///
    /// 与 [`LoopDetector::record_tool_call`] 的工具维度正交:后者检测
    /// "连续无文件修改",本方法检测"连续纠结同一问题"。
    #[must_use]
    pub fn record_thinking(&mut self, thinking: &str) -> LoopAction {
        let stalled = COG_STALL_MARKERS.iter().any(|m| thinking.contains(m));
        if !stalled {
            // 本轮思考是推进性的(不含纠结标记)→ 清零,认知停滞重新累计。
            self.cog_stall_run = 0;
            self.cog_stall_warned = false;
            return LoopAction::Continue;
        }
        self.cog_stall_run += 1;
        if self.cog_stall_run >= COG_STALL_WARN_THRESHOLD && !self.cog_stall_warned {
            self.cog_stall_warned = true;
            return LoopAction::InjectContext(COG_STALL_TRACE_PROMPT.to_string());
        }
        LoopAction::Continue
    }

    /// 重置文件编辑跟踪(每个 turn 开始调用;工具调用计数保留,支持跨 turn 检测)。
    ///
    /// 同时重置无产出 streak:no-output 检测限定在单 turn 内,避免跨 turn 只读
    /// 探索被线性累加误判,以及 abort 后下一 turn 因 streak 未清而反复触发。
    /// 认知停滞同样每 turn 重置(单 turn 内现象,跨 turn 累积会误报)。
    pub fn reset_edits(&mut self) {
        self.edit_counts.clear();
        self.warned.clear();
        self.total_edits = 0;
        self.no_output_run = 0;
        self.no_output_warned = false;
        self.cog_stall_run = 0;
        self.cog_stall_warned = false;
    }

    /// 按时间窗口衰减工具调用计数:超过 `max_age_ms` 未出现的调用从统计中移除。
    /// 工具调用跨 turn 保留(窗口内);文件编辑计数不受影响(每 turn 由
    /// [`LoopDetector::reset_edits`] 清空)。
    pub fn prune_decayed(&mut self, now_ms: u64, max_age_ms: u64) {
        self.tool_call_counts
            .retain(|_, (_, last_seen)| now_ms.saturating_sub(*last_seen) <= max_age_ms);
        self.tool_output_counts
            .retain(|_, (_, last_seen)| now_ms.saturating_sub(*last_seen) <= max_age_ms);
        self.tool_warned
            .retain(|_, warned_at| now_ms.saturating_sub(*warned_at) <= max_age_ms);
    }
    #[must_use]
    pub fn edit_count(&self, file_path: &str) -> u32 {
        self.edit_counts.get(file_path).copied().unwrap_or(0)
    }

    /// 获取累计总编辑次数。
    #[must_use]
    pub fn total_edits(&self) -> u64 {
        self.total_edits
    }

    /// 校验 MCP Tools 注册数量是否在经验法则范围内。
    ///
    /// 返回 `Ok(())` 如果在范围内,否则返回 `Err` 含超量信息。
    pub fn validate_mcp_tools_count(count: usize) -> Result<(), String> {
        if count <= MCP_TOOLS_MAX {
            Ok(())
        } else {
            Err(format!(
                "MCP tools count {count} exceeds recommended maximum {MCP_TOOLS_MAX}"
            ))
        }
    }

    /// 校验 Skills 注册数量是否在经验法则范围内。
    pub fn validate_skills_count(count: usize) -> Result<(), String> {
        if count <= SKILLS_MAX {
            Ok(())
        } else {
            Err(format!(
                "Skills count {count} exceeds recommended maximum {SKILLS_MAX}"
            ))
        }
    }
}

/// 规范化工具输入,使语义相同的调用互相匹配:
/// - JSON 输入 → 键序无关(`{"b":1,"a":2}` ≡ `{"a":2,"b":1}`)
/// - 非 JSON 输入 → 压缩连续空白
fn normalize_tool_input(input: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(input) {
        return value.to_string();
    }
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 规范化工具输出,使"语义相同、文本不同"的调用互相匹配(验证循环检测):
/// - 剥离时间戳类 token(ISO-8601 / 时钟时间,如 `2026-08-09T01:26:43.123Z`)→ `TS`
/// - 折叠连续空白(含 `\r\n` → 单个空格)
///
/// 启发式:`is_timestamp_like` 判定 token 是否"长度 ≥ 8、全由数字与
/// `:-TZ.+` 组成且含至少一个分隔符"。纯数字(端口号等)不受影响。
fn normalize_output(output: &str) -> String {
    let mut out = String::with_capacity(output.len());
    let mut token = String::new();
    let flush = |out: &mut String, token: &mut String| {
        if !token.is_empty() {
            if is_timestamp_like(token) {
                out.push_str("TS");
            } else {
                out.push_str(token);
            }
            token.clear();
        }
    };
    for ch in output.chars() {
        if ch.is_whitespace() {
            flush(&mut out, &mut token);
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            token.push(ch);
        }
    }
    flush(&mut out, &mut token);
    out.trim().to_string()
}

/// 启发式:token 是否像时间戳(长度 ≥ 8,仅由数字与分隔符组成,含至少 1 个分隔符)。
fn is_timestamp_like(token: &str) -> bool {
    if token.chars().count() < 8 {
        return false;
    }
    let mut digits = 0usize;
    let mut seps = 0usize;
    for c in token.chars() {
        match c {
            '0'..='9' => digits += 1,
            ':' | '-' | 'T' | 'Z' | '.' | '+' => seps += 1,
            _ => return false,
        }
    }
    digits >= 8 && seps >= 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_edit_returns_continue_below_warn_threshold() {
        let mut detector = LoopDetector::new();
        for i in 0..WARN_THRESHOLD - 1 {
            let action = detector.record_edit("src/main.rs");
            assert!(
                matches!(action, LoopAction::Continue),
                "expected Continue at edit {i}, got {action:?}"
            );
        }
    }

    #[test]
    fn record_edit_returns_inject_context_at_warn_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..WARN_THRESHOLD - 1 {
            let _ = detector.record_edit("src/main.rs");
        }
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::InjectContext(_)));
        if let LoopAction::InjectContext(msg) = action {
            assert!(msg.contains("reconsidering"));
        }
    }

    #[test]
    fn record_edit_returns_abort_at_abort_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..ABORT_THRESHOLD - 1 {
            let _ = detector.record_edit("src/main.rs");
        }
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::Abort(_)));
        if let LoopAction::Abort(msg) = action {
            assert!(msg.contains("doom loop detected"));
            assert!(msg.contains("src/main.rs"));
        }
    }

    #[test]
    fn different_files_tracked_independently() {
        let mut detector = LoopDetector::new();
        for _ in 0..WARN_THRESHOLD {
            let _ = detector.record_edit("src/a.rs");
        }
        // a.rs 已达 WARN,但 b.rs 仍 Continue
        let action = detector.record_edit("src/b.rs");
        assert!(matches!(action, LoopAction::Continue));
        assert_eq!(detector.edit_count("src/a.rs"), WARN_THRESHOLD);
        assert_eq!(detector.edit_count("src/b.rs"), 1);
    }

    #[test]
    fn reset_clears_counts() {
        let mut detector = LoopDetector::new();
        for _ in 0..WARN_THRESHOLD + 2 {
            let _ = detector.record_edit("src/main.rs");
        }
        detector.reset_edits();
        assert_eq!(detector.edit_count("src/main.rs"), 0);
        assert_eq!(detector.total_edits(), 0);
        // After reset, edits should start fresh
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::Continue));
    }

    #[test]
    fn warn_only_injected_once_per_file() {
        let mut detector = LoopDetector::new();
        // Edit up to WARN_THRESHOLD → InjectContext
        for _ in 0..WARN_THRESHOLD {
            let _ = detector.record_edit("src/main.rs");
        }
        // Next edit (WARN+1) should be Continue, not another InjectContext
        let action = detector.record_edit("src/main.rs");
        assert!(
            matches!(action, LoopAction::Continue),
            "expected Continue after warn already emitted, got {action:?}"
        );
    }

    #[test]
    fn abort_triggers_on_every_edit_above_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..ABORT_THRESHOLD {
            let _ = detector.record_edit("src/main.rs");
        }
        // ABORT_THRESHOLD + 1 should also abort
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::Abort(_)));
    }

    #[test]
    fn total_edits_tracks_across_files() {
        let mut detector = LoopDetector::new();
        let _ = detector.record_edit("a.rs");
        let _ = detector.record_edit("b.rs");
        let _ = detector.record_edit("a.rs");
        assert_eq!(detector.total_edits(), 3);
    }

    #[test]
    fn validate_mcp_tools_count_within_limit() {
        assert!(LoopDetector::validate_mcp_tools_count(80).is_ok());
        assert!(LoopDetector::validate_mcp_tools_count(0).is_ok());
    }

    #[test]
    fn validate_mcp_tools_count_exceeds_limit() {
        assert!(LoopDetector::validate_mcp_tools_count(81).is_err());
    }
    #[test]
    fn validate_skills_count_within_limit() {
        assert!(LoopDetector::validate_skills_count(15).is_ok());
    }

    #[test]
    fn validate_skills_count_exceeds_limit() {
    }

    // -----------------------------------------------------------------
    // 工具调用循环检测(§修复:Bug-2 bash 诊断循环无防护)
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------

    #[test]
    fn record_tool_call_continues_below_warn_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..TOOL_WARN_THRESHOLD - 1 {
            let action = detector.record_tool_call("Bash", "netstat -an | grep 62112", "LISTENING");
            assert!(matches!(action, LoopAction::Continue));
        }
    }

    #[test]
    fn record_tool_call_injects_context_at_warn_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..TOOL_WARN_THRESHOLD - 1 {
            let _ = detector.record_tool_call("Bash", "tail -f log", "");
        }
        let action = detector.record_tool_call("Bash", "tail -f log", "");
        assert!(matches!(action, LoopAction::InjectContext(_)));
        if let LoopAction::InjectContext(msg) = action {
            assert!(msg.contains("identical input"));
        }
    }

    #[test]
    fn record_tool_call_aborts_at_abort_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..TOOL_ABORT_THRESHOLD {
            let _ = detector.record_tool_call("Bash", "netstat -an", "LISTENING");
        }
        let action = detector.record_tool_call("Bash", "netstat -an", "LISTENING");
        assert!(matches!(action, LoopAction::Abort(_)));
        if let LoopAction::Abort(msg) = action {
            assert!(msg.contains("doom loop detected"));
        }
    }

    #[test]
    fn different_inputs_same_output_detected() {
        // 输入不同但输出相同的验证循环(sleep 3/5/8 && tail 输出全空)
        let mut detector = LoopDetector::new();
        for i in 0..SAME_OUTPUT_WARN_THRESHOLD - 1 {
            let input = format!("sleep {} && tail im-bridge.log", i * 2 + 1);
            let action = detector.record_tool_call("Bash", &input, "");
            assert!(matches!(action, LoopAction::Continue));
        }
        let action = detector.record_tool_call("Bash", "sleep 9 && tail im-bridge.log", "");
        assert!(matches!(action, LoopAction::InjectContext(_)));
        if let LoopAction::InjectContext(msg) = action {
            assert!(msg.contains("identical output"));
        }
    }

    #[test]
    fn identical_output_aborts_at_higher_threshold() {
        let mut detector = LoopDetector::new();
        for i in 0..SAME_OUTPUT_ABORT_THRESHOLD - 1 {
            let input = format!("step{i}");
            let _ = detector.record_tool_call("Bash", &input, "status: ok");
        }
        let action = detector.record_tool_call("Bash", "step-final", "status: ok");
        assert!(matches!(action, LoopAction::Abort(_)));
    }

    #[test]
    fn json_input_normalization_matches_key_order_agnostic() {
        let mut detector = LoopDetector::new();
        // 键序不同的 JSON 视为相同调用
        let a = detector.record_tool_call("Read", r#"{"file_path":"/x","offset":1}"#, "abc");
        let b = detector.record_tool_call("Read", r#"{"offset":1,"file_path":"/x"}"#, "abc");
        assert!(matches!(a, LoopAction::Continue));
        assert!(matches!(b, LoopAction::Continue));
        // 第三次(键序再次不同)应触发警告 —— 证明已按规范化 input 归并
        let c = detector.record_tool_call("Read", r#"{"offset":1,"file_path":"/x"}"#, "abc");
        assert!(matches!(c, LoopAction::InjectContext(_)));
    }

    #[test]
    fn reset_edits_preserves_tool_call_counts() {
        let mut detector = LoopDetector::new();
        for _ in 0..TOOL_WARN_THRESHOLD - 1 {
            let _ = detector.record_tool_call("Bash", "netstat", "x");
        }
        detector.reset_edits();
        // reset_edits 只清文件编辑计数,tool 调用计数保留(跨 turn 检测)
        let action = detector.record_tool_call("Bash", "netstat", "x");
        assert!(
            matches!(action, LoopAction::InjectContext(_)),
            "reset_edits 后 tool 计数应保留(第 3 次触发警告): {action:?}"
        );
    }

    #[test]
    fn reset_edits_resets_no_output_streak() {
        // abort 后 no_output_run 未清是反复触发的根因;reset_edits 应清零
        // 无产出 streak,使下一 turn 从零重新累计(不再立即警告/中止)。
        let mut detector = LoopDetector::new();
        for i in 0..NO_OUTPUT_WARN_THRESHOLD {
            let _ = detector.record_tool_call("Bash", &format!("probe {i}"), &format!("out {i}"));
        }
        detector.reset_edits();
        // 重置后重新累计到接近警告阈值,前几次应仍为 Continue
        for i in 0..NO_OUTPUT_WARN_THRESHOLD - 1 {
            let action = detector.record_tool_call("Bash", &format!("post {i}"), &format!("out {i}"));
            assert!(
                matches!(action, LoopAction::Continue),
                "reset_edits 后无产出 streak 应清零,第 {i} 次应仍为 Continue: {action:?}"
            );
        }
    }

    #[test]
    fn tool_loop_detection_does_not_interfere_with_file_edits() {
        let mut detector = LoopDetector::new();
        // 工具调用计数不影响文件编辑计数
        for _ in 0..TOOL_ABORT_THRESHOLD {
            let _ = detector.record_tool_call("Bash", "ls", "");
        }
        assert_eq!(detector.edit_count("src/main.rs"), 0);
        let action = detector.record_edit("src/main.rs");
        assert!(matches!(action, LoopAction::Continue));
    }

    // -----------------------------------------------------------------
    // 无产出通道(探索循环):连续非文件修改工具调用检测
    // -----------------------------------------------------------------

    #[test]
    fn no_output_streak_warns_then_aborts() {
        // 每次调用不同只读工具(绕过相同-input / 相同-output 通道),
        // 但从不产出 → 第 NO_OUTPUT_WARN_THRESHOLD 次警告,第
        // NO_OUTPUT_ABORT_THRESHOLD 次中止。
        let mut detector = LoopDetector::new();
        for i in 0..NO_OUTPUT_WARN_THRESHOLD - 1 {
            let input = format!("probe step {i}");
            let output = format!("output #{i}");
            let action = detector.record_tool_call("Bash", &input, &output);
            assert!(
                matches!(action, LoopAction::Continue),
                "第 {i} 次调用应仍为 Continue(低于警告阈值): {action:?}"
            );
        }
        let action = detector.record_tool_call("Bash", "probe step warn", "output #warn");
        assert!(
            matches!(action, LoopAction::InjectContext(_)),
            "第 {NO_OUTPUT_WARN_THRESHOLD} 次连续无产出调用应触发警告: {action:?}"
        );
        if let LoopAction::InjectContext(msg) = action {
            assert!(msg.contains("no file modification"));
        }
        // 继续到中止阈值
        for i in NO_OUTPUT_WARN_THRESHOLD..NO_OUTPUT_ABORT_THRESHOLD - 1 {
            let input = format!("probe step {i}");
            let output = format!("output #{i}");
            let action = detector.record_tool_call("Bash", &input, &output);
            assert!(
                matches!(action, LoopAction::Continue),
                "第 {i} 次调用应仍为 Continue(警告后不再重复注入): {action:?}"
            );
        }
        let action = detector.record_tool_call("Bash", "probe step abort", "output #abort");
        assert!(
            matches!(action, LoopAction::Abort(_)),
            "第 {NO_OUTPUT_ABORT_THRESHOLD} 次连续无产出调用应中止: {action:?}"
        );
        if let LoopAction::Abort(msg) = action {
            assert!(msg.contains("no-output loop detected"));
        }
    }

    #[test]
    fn file_edit_resets_no_output_streak() {
        // 无产出 streak 中插入一次文件编辑 → 计数清零,重新累计
        let mut detector = LoopDetector::new();
        for i in 0..NO_OUTPUT_WARN_THRESHOLD - 1 {
            let _ = detector.record_tool_call("Bash", &format!("probe {i}"), &format!("out {i}"));
        }
        // 文件编辑(record_edit)清零 streak
        let _ = detector.record_edit("src/main.rs");
        // 重新累计到警告阈值才会警告(而非依赖旧的 streak)
        for i in 0..NO_OUTPUT_WARN_THRESHOLD - 1 {
            let action =
                detector.record_tool_call("Read", &format!("file {i}"), &format!("content {i}"));
            assert!(matches!(action, LoopAction::Continue));
        }
        let action = detector.record_tool_call("Read", "file warn", "content warn");
        assert!(
            matches!(action, LoopAction::InjectContext(_)),
            "编辑后 streak 应重置,需重新达到警告阈值: {action:?}"
        );
    }

    #[test]
    fn file_modifying_tool_resets_no_output_streak() {
        // 直接调用文件修改工具(走 record_tool_call)同样清零 streak
        let mut detector = LoopDetector::new();
        for i in 0..NO_OUTPUT_WARN_THRESHOLD {
            let _ = detector.record_tool_call("Bash", &format!("probe {i}"), &format!("out {i}"));
        }
        let action = detector.record_tool_call(
            "edit_file",
            r#"{"file_path":"src/main.rs","old_string":"a","new_string":"b"}"#,
            "ok",
        );
        assert!(
            matches!(action, LoopAction::Continue),
            "文件修改工具应清零 streak 并返回 Continue: {action:?}"
        );
        // 之后重新累计:前几次不再立即警告
        for i in 0..NO_OUTPUT_WARN_THRESHOLD - 1 {
            let action =
                detector.record_tool_call("Read", &format!("file {i}"), &format!("content {i}"));
            assert!(matches!(action, LoopAction::Continue));
        }
    }

    #[test]
    fn text_output_resets_no_output_streak() {
        // 模型输出文本(弱产出信号)清零 streak —— 保护"边说明边探索"的复杂诊断
        let mut detector = LoopDetector::new();
        // 累计到接近警告阈值
        for i in 0..NO_OUTPUT_WARN_THRESHOLD - 1 {
            let _ = detector.record_tool_call("Read", &format!("file {i}"), &format!("content {i}"));
        }
        // 模型输出文本 → streak 清零
        detector.record_text_output();
        // 重新累计到警告阈值才警告(而非依赖旧 streak)
        for i in 0..NO_OUTPUT_WARN_THRESHOLD - 1 {
            let action =
                detector.record_tool_call("Bash", &format!("probe {i}"), &format!("out {i}"));
            assert!(matches!(action, LoopAction::Continue));
        }
        let action = detector.record_tool_call("Bash", "probe warn", "out warn");
        assert!(
            matches!(action, LoopAction::InjectContext(_)),
            "文本输出后 streak 应重置,需重新达到警告阈值: {action:?}"
        );
    }

    // -----------------------------------------------------------------
    // Task 3:跨 turn 衰减 + 输出规范化
    // -----------------------------------------------------------------

    #[test]
    fn tool_call_counts_survive_across_turns_within_window() {
        // 模拟跨 turn:turn1 记录 3 次,turn2(仍在窗口内)继续累积到中止阈值
        let mut detector = LoopDetector::new();
        let now = 1_000_000u64;
        for _ in 0..TOOL_WARN_THRESHOLD {
            let _ = detector.record_tool_call_at("Bash", "netstat", "LISTENING", now);
        }
        // 下一 turn,5 分钟后(仍在 15 分钟窗口内)继续相同调用
        let mut aborted = false;
        // 仅补到第 5 次(4、5 次应为 Continue/InjectContext),第 6 次留给循环外的最终断言
        for _ in TOOL_WARN_THRESHOLD..TOOL_ABORT_THRESHOLD - 1 {
            let action = detector
                .record_tool_call_at("Bash", "netstat", "LISTENING", now + 5 * 60 * 1000);
            if matches!(action, LoopAction::Abort(_)) {
                aborted = true;
            }
        }
        assert!(!aborted, "第 4-5 次应仍为 Continue/InjectContext");
        let action = detector.record_tool_call_at("Bash", "netstat", "LISTENING", now + 5 * 60 * 1000);
        assert!(
            matches!(action, LoopAction::Abort(_)),
            "跨 turn 循环(窗口内)应被检测: {action:?}"
        );
    }

    #[test]
    fn prune_decayed_removes_stale_tool_calls() {
        let mut detector = LoopDetector::new();
        let now = 1_000_000u64;
        for _ in 0..TOOL_WARN_THRESHOLD {
            let _ = detector.record_tool_call_at("Bash", "netstat", "", now);
        }
        // 时间流逝超过窗口 → 计数清空
        detector.prune_decayed(now + 20 * 60 * 1000, 15 * 60 * 1000);
        // 重新计数:前 2 次仍 Continue(而非立即警告/中止)
        for _ in 0..TOOL_WARN_THRESHOLD - 1 {
            let action =
                detector.record_tool_call_at("Bash", "netstat", "", now + 21 * 60 * 1000);
            assert!(
                matches!(action, LoopAction::Continue),
                "prune 后计数应重置: {action:?}"
            );
        }
    }

    #[test]
    fn normalize_output_strips_timestamps_and_whitespace() {
        assert_eq!(normalize_output("2026-08-09T01:26:43.123Z listening"), "TS listening");
        assert_eq!(normalize_output("   abc   def  \r\n "), "abc def");
        assert_eq!(normalize_output("error 404: page not found"), "error 404: page not found");
        assert_eq!(normalize_output("62112"), "62112"); // 纯数字端口号不受影响
    }

    #[test]
    fn identical_output_with_different_timestamps_is_detected() {
        // tail -f 日志:每条输出带不同时间戳 → 规范化后视为相同输出,触发验证循环检测
        let mut detector = LoopDetector::new();
        for i in 0..SAME_OUTPUT_WARN_THRESHOLD - 1 {
            let out = format!("2026-08-09T01:26:{i:02}.123Z still waiting");
            let _ = detector.record_tool_call("Bash", &format!("sleep {} && tail log", i), &out);
        }
        let action = detector.record_tool_call(
            "Bash",
            "sleep 9 && tail log",
            "2026-08-09T02:00:00.000Z still waiting",
        );
        assert!(
            matches!(action, LoopAction::InjectContext(_)),
            "时间戳不同的相同输出应触发警告: {action:?}"
        );
    }

    // -----------------------------------------------------------------
    // 认知停滞通道:连续纠结 thinking 检测(认知维度)
    // -----------------------------------------------------------------

    #[test]
    fn record_thinking_continues_below_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..COG_STALL_WARN_THRESHOLD - 1 {
            let action = detector.record_thinking("等等,这里需要确认,让我再想一下");
            assert!(
                matches!(action, LoopAction::Continue),
                "低于阈值应仍为 Continue: {action:?}"
            );
        }
    }

    #[test]
    fn record_thinking_injects_at_threshold() {
        let mut detector = LoopDetector::new();
        for _ in 0..COG_STALL_WARN_THRESHOLD - 1 {
            let _ = detector.record_thinking("这个无法确定,需要确认");
        }
        let action = detector.record_thinking("我不能凭空猜测,让我再想");
        assert!(matches!(action, LoopAction::InjectContext(_)));
        if let LoopAction::InjectContext(msg) = action {
            assert!(msg.contains("不确定性溯源"), "提示应含溯源指令: {msg}");
        }
    }

    #[test]
    fn record_thinking_clears_on_progressive_thought() {
        // 纠结几轮后出现一轮推进性思考(不含标记词)→ 计数清零,重新累计
        let mut detector = LoopDetector::new();
        for _ in 0..COG_STALL_WARN_THRESHOLD - 1 {
            let _ = detector.record_thinking("这个不确定,让我再想");
        }
        // 一轮推进性思考 → 清零
        let action = detector.record_thinking("读取文件后确认了数据结构,继续实现");
        assert!(matches!(action, LoopAction::Continue));
        // 重新累计:前几轮不再立即触发
        for _ in 0..COG_STALL_WARN_THRESHOLD - 1 {
            let action = detector.record_thinking("还是不确定,需要确认");
            assert!(
                matches!(action, LoopAction::Continue),
                "清零后应重新累计: {action:?}"
            );
        }
    }

    #[test]
    fn record_thinking_warns_only_once() {
        let mut detector = LoopDetector::new();
        for _ in 0..COG_STALL_WARN_THRESHOLD {
            let _ = detector.record_thinking("无法确定,需要确认");
        }
        // 已触发后继续命中 → 不再重复注入
        let action = detector.record_thinking("还是不确定,让我再想");
        assert!(
            matches!(action, LoopAction::Continue),
            "触发后应不再重复注入: {action:?}"
        );
    }

    #[test]
    fn reset_edits_resets_cog_stall() {
        // 认知停滞跨 turn 累积会误报;reset_edits 应清零,使下一 turn 重新累计
        let mut detector = LoopDetector::new();
        for _ in 0..COG_STALL_WARN_THRESHOLD {
            let _ = detector.record_thinking("不确定,需要确认");
        }
        detector.reset_edits();
        for _ in 0..COG_STALL_WARN_THRESHOLD - 1 {
            let action = detector.record_thinking("让我再想,不确定");
            assert!(
                matches!(action, LoopAction::Continue),
                "reset_edits 后认知停滞应清零: {action:?}"
            );
        }
    }

    #[test]
    fn cog_stall_lesson_is_generic_and_short() {
        // 第 2 层教训必须是通用失败模式(不绑定具体领域术语,跨任务复用),
        // 且长度 ≤ 200 字符(lessons::LESSON_MAX_CHARS),避免被截断丢语义。
        assert!(!COG_STALL_LESSON.is_empty());
        assert!(COG_STALL_LESSON.chars().count() <= 200);
        // 通用性铁律:不含任何具体领域术语(如"中枢/刺穿/缠论")
        assert!(!COG_STALL_LESSON.contains("中枢"));
        assert!(!COG_STALL_LESSON.contains("刺穿"));
        assert!(!COG_STALL_LESSON.contains("缠论"));
    }
}
