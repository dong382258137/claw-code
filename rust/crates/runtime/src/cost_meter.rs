//! 成本反馈注记(Cost Feedback Notes,P2 · Task 5「让模型看见自己的燃烧率」)。
//!
//! 问题:runtime 消耗的 output token 只有模型自己烧得出,但模型看不见账单 ——
//! 成本开环。典型场景:Trivial 档轻量问答单 turn 烧掉 43K output 仍不自知。
//! 本模块提供会话级成本累计器 [`CostMeter`]:运行时把每条 API 响应的
//! `output_tokens` 与已执行工具调用数累计进 meter,在工具循环迭代末尾
//! 评估是否需要注入 `[runtime]` 成本注记(user 消息,下一次请求构造对模型
//! 可见),让模型第一次能看见"本 turn 已消耗 output ~n tokens、工具调用
//! m 次";超阈值(默认 20K,env `CLAW_COST_WARN_OUTPUT_THRESHOLD` 可覆盖)
//! 时注入升级版警示,引导立即收敛。
//!
//! 设计原则(与 Task 4 充分性契约对齐):
//! - **零 LLM**:累计与决策纯确定性,不消耗 token。
//! - **非关键路径**:注记注入失败静默(与既有 `[runtime]` 提醒一致)。
//! - **注记本身也有成本**:注入频率设上限(普通提示每 ~5 次工具调用至多
//!   一条、警示每 ~10K output 至多一条),防刷屏变成新的成本源。
//! - **Trivial/Standard 两档都生效**:成本失控在 Trivial(轻量问答)场景最
//!   严重,不按档位豁免,也不需要读档位。

/// 普通成本提示的 output 触发线:本 turn 累计 output ≥ 此值即值得提醒一次。
pub const SOFT_NOTE_OUTPUT_THRESHOLD: u64 = 5_000;

/// 普通成本提示的最低工具往返数:本 turn 工具调用 ≥ 此值且 output > 0 也提示
/// ("工具调用 ≥ 3 次且 output 仍增长" —— 来回折腾本身是燃烧信号)。
pub const SOFT_NOTE_MIN_TOOL_CALLS: usize = 3;

/// 普通成本提示频率上限:距上次提示 ≥ 此值(次工具调用)才允许再提示一条。
pub const SOFT_NOTE_TOOL_INTERVAL: usize = 5;

/// 高燃烧警示阈值:本 turn 累计 output **达到**此值即注入升级版警示
/// (用 `>=` 而非 `>` —— 到达 20K 的当轮就应警示,模型下一轮请求前可见,
/// 避免 20K..=20K+ε 窗口漏报;`>` 会把触发推迟到下一次 usage 增量,迟一轮
/// 收敛提示)。env `CLAW_COST_WARN_OUTPUT_THRESHOLD` 可覆盖。
pub const COST_WARN_OUTPUT_THRESHOLD: u64 = 20_000;

/// 警示频率上限:距上次警示 ≥ 此值(output tokens)才允许再次警示。
pub const WARN_OUTPUT_INTERVAL: u64 = 10_000;

/// 覆盖警示阈值的环境变量名(`CLAW_COST_WARN_OUTPUT_THRESHOLD`)。
pub const COST_WARN_OUTPUT_THRESHOLD_ENV: &str = "CLAW_COST_WARN_OUTPUT_THRESHOLD";

/// 读取高燃烧警示阈值(env 可覆盖;非法/低于下限 1K 时回退默认 20K)。
#[must_use]
pub fn cost_warn_output_threshold() -> u64 {
    std::env::var(COST_WARN_OUTPUT_THRESHOLD_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n >= 1_000)
        .unwrap_or(COST_WARN_OUTPUT_THRESHOLD)
}

/// 注记类型(纯决策函数 [`decide_note`] 的输出)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostNoteKind {
    /// 普通成本提示:提醒模型"看得见燃烧、避免追加低价值调用"。
    Hint,
    /// 升级版成本警示:output 已达高燃烧阈值,建议立即收敛。
    Warn,
}

/// 串行化"触碰 `CLAW_COST_WARN_OUTPUT_THRESHOLD` env"的测试。
///
/// 与既有 `ENV_LOCK`(CLAUDE_SURVEY_*)同模式:设置 env 的测试与依赖默认
/// 阈值的集成测试并行运行会互相污染,须共享同一把锁。`#[cfg(test)]` 下
/// 定义,conversation.rs 集成测试经 `crate::cost_meter::COST_ENV_LOCK`
/// 引用(仅测试构建存在)。
#[cfg(test)]
pub(crate) static COST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 会话级成本累计器(挂在 [`crate::conversation::ConversationRuntime`] 上)。
///
/// 生命周期:turn 开头 [`reset_turn`](CostMeter::reset_turn)(turn 级计数与
/// 注记位点清零,全会话累计保留);每条 API 响应 usage 到达时
/// [`record_output`](CostMeter::record_output);每个工具批处理开始时
/// [`add_tool_calls`](CostMeter::add_tool_calls);每个工具批处理迭代末尾
/// [`poll_note`](CostMeter::poll_note) 决策是否注入注记。
///
/// 注记频率上限(turn 内):普通提示距上次提示 ≥ `SOFT_NOTE_TOOL_INTERVAL` 次
/// 工具调用;警示距上次警示 ≥ `WARN_OUTPUT_INTERVAL` tokens。两个位点独立
/// 记录;警示注入同时占用一次普通提示额度(推迟下一条提示),避免连发。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CostMeter {
    /// 本 turn 累计 output tokens(turn 开头清零)。
    turn_output_tokens: u64,
    /// 全会话累计 output tokens(跨 turn 持续;口径与 digest 的
    /// `usage_tracker.cumulative_usage().output_tokens` 一致,仅放宽为 u64)。
    total_output_tokens: u64,
    /// 本 turn 已发起(含执行/被拒)的工具调用数,按模型声明的 tool_use 计。
    turn_tool_calls: usize,
    /// 全会话累计工具调用数。
    total_tool_calls: usize,
    /// 上次普通提示时的 `turn_tool_calls`。`0` = 本 turn 尚未提示过
    /// (提示只可能在 `turn_tool_calls >= 1` 后发生,故 0 无歧义)。
    last_hint_tool_calls: usize,
    /// 上次警示时的 `turn_output_tokens`。`0` = 本 turn 尚未警示过。
    last_warn_output: u64,
}

impl CostMeter {
    /// 累加一条 API 响应的 output tokens(本 turn 与全会话同时累计)。
    pub fn record_output(&mut self, output_tokens: u64) {
        self.turn_output_tokens = self.turn_output_tokens.saturating_add(output_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(output_tokens);
    }

    /// 累加一批工具调用(按模型声明的 tool_use 数;执行与否都算一次往返)。
    pub fn add_tool_calls(&mut self, count: usize) {
        self.turn_tool_calls = self.turn_tool_calls.saturating_add(count);
        self.total_tool_calls = self.total_tool_calls.saturating_add(count);
    }

    /// turn 开头复位:清零 turn 级计数与注记位点;全会话累计保留(跨 turn)。
    pub fn reset_turn(&mut self) {
        self.turn_output_tokens = 0;
        self.turn_tool_calls = 0;
        self.last_hint_tool_calls = 0;
        self.last_warn_output = 0;
    }

    /// 当前 turn 累计 output tokens(只读;供收敛契约门控读取燃烧状态)。
    #[must_use]
    pub fn turn_output_tokens(&self) -> u64 {
        self.turn_output_tokens
    }

    /// 当前 turn 累计工具调用数(只读;供收敛契约门控读取)。
    #[must_use]
    pub fn turn_tool_calls(&self) -> usize {
        self.turn_tool_calls
    }

    /// 工具批处理迭代末尾调用:决策 + 更新注记位点 + 渲染注记文本。
    ///
    /// 返回需要注入的 `[runtime]` 注记(至多一条);`None` = 本轮不注记。
    /// 位点更新保证同一 turn 内普通提示每 ~5 次工具调用一条、警示每 ~10K
    /// output 一条,不会在连续迭代中反复注入。
    pub fn poll_note(&mut self, warn_threshold: u64) -> Option<String> {
        match decide_note(
            self.turn_output_tokens,
            self.turn_tool_calls,
            self.last_hint_tool_calls,
            self.last_warn_output,
            warn_threshold,
        ) {
            Some(CostNoteKind::Warn) => {
                self.last_warn_output = self.turn_output_tokens;
                // 警示比提示更重:同时占用普通提示额度,避免警示后紧接着提示。
                self.last_hint_tool_calls = self.turn_tool_calls;
                Some(render_warning(self.turn_output_tokens))
            }
            Some(CostNoteKind::Hint) => {
                self.last_hint_tool_calls = self.turn_tool_calls;
                Some(render_hint(self.turn_output_tokens, self.turn_tool_calls))
            }
            None => None,
        }
    }
}

/// 纯决策:给定当前 turn 累计 output / 工具调用数与上次注记位点,返回应注入
/// 的注记类型(至多一种)。不修改任何状态,便于单测阈值与频率上限。
///
/// 决策顺序(警示优先):
/// 1. **警示**:`turn_output_tokens >= warn_threshold`(达到即触发,见常量
///    注释的边界说明)且距上次警示 ≥ `WARN_OUTPUT_INTERVAL` tokens;
/// 2. **普通提示**:有真实燃烧(`turn_output_tokens > 0`,usage 缺失时提示
///    "~0 tokens" 无意义)且(累计 output ≥ 5K 或工具往返 ≥ 3 次)且距上次
///    提示 ≥ 5 次工具调用(未提示过则直接放行)。
///
/// 首轮 / 数据为 0(`turn_output_tokens == 0`)恒返回 `None`,不注记。
#[must_use]
pub fn decide_note(
    turn_output_tokens: u64,
    turn_tool_calls: usize,
    last_hint_tool_calls: usize,
    last_warn_output: u64,
    warn_threshold: u64,
) -> Option<CostNoteKind> {
    // 警示优先:高燃烧是比"多查了几次"更强的收敛信号。
    if turn_output_tokens >= warn_threshold
        && turn_output_tokens.saturating_sub(last_warn_output) >= WARN_OUTPUT_INTERVAL
    {
        return Some(CostNoteKind::Warn);
    }
    // 普通提示。
    let reached_baseline = turn_output_tokens > 0
        && (turn_output_tokens >= SOFT_NOTE_OUTPUT_THRESHOLD
            || turn_tool_calls >= SOFT_NOTE_MIN_TOOL_CALLS);
    let fresh_enough = last_hint_tool_calls == 0
        || turn_tool_calls.saturating_sub(last_hint_tool_calls) >= SOFT_NOTE_TOOL_INTERVAL;
    if reached_baseline && fresh_enough {
        return Some(CostNoteKind::Hint);
    }
    None
}

/// 渲染普通成本提示注记文本。
#[must_use]
pub fn render_hint(turn_output_tokens: u64, turn_tool_calls: usize) -> String {
    format!(
        "[runtime] 成本提示:本 turn 已消耗 output ~{turn_output_tokens} tokens、\
         工具调用 {turn_tool_calls} 次。若已接近结论请直接作答,避免为低价值验证追加调用。"
    )
}

/// 渲染升级版成本警示注记文本。
#[must_use]
pub fn render_warning(turn_output_tokens: u64) -> String {
    format!(
        "[runtime] 成本警示:本 turn output 已超 {turn_output_tokens} tokens(高燃烧)。\
         建议立即收敛:若判据已满足,直接输出最终答案;确需更多信息请先询问用户方向。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 累计器:多条 usage 分别累计进 turn 与 total;reset_turn 只清 turn。
    #[test]
    fn accumulates_turn_and_total_output_separately() {
        let mut meter = CostMeter::default();
        meter.record_output(1_000);
        meter.record_output(2_000);
        assert_eq!(meter.turn_output_tokens, 3_000);
        assert_eq!(meter.total_output_tokens, 3_000);

        meter.reset_turn();
        assert_eq!(meter.turn_output_tokens, 0);
        assert_eq!(meter.total_output_tokens, 3_000, "全会话累计跨 turn 保留");

        meter.record_output(500);
        assert_eq!(meter.turn_output_tokens, 500);
        assert_eq!(meter.total_output_tokens, 3_500);
    }

    /// 累计器:工具调用 turn 与 total 分开;reset_turn 只清 turn。
    #[test]
    fn accumulates_turn_and_total_tool_calls_separately() {
        let mut meter = CostMeter::default();
        meter.add_tool_calls(2);
        meter.add_tool_calls(1);
        assert_eq!(meter.turn_tool_calls, 3);
        assert_eq!(meter.total_tool_calls, 3);

        meter.reset_turn();
        meter.add_tool_calls(1);
        assert_eq!(meter.turn_tool_calls, 1);
        assert_eq!(meter.total_tool_calls, 4, "全会话工具计数跨 turn 保留");
    }

    /// 0 output(usage 缺失)不注入 —— 提示 "~0 tokens" 无意义,首轮同理。
    #[test]
    fn zero_output_never_notes_even_with_many_tool_calls() {
        assert_eq!(decide_note(0, 0, 0, 0, COST_WARN_OUTPUT_THRESHOLD), None);
        assert_eq!(decide_note(0, 20, 0, 0, COST_WARN_OUTPUT_THRESHOLD), None);
        let mut meter = CostMeter::default();
        for _ in 0..10 {
            meter.add_tool_calls(1);
            assert_eq!(meter.poll_note(COST_WARN_OUTPUT_THRESHOLD), None);
        }
    }

    /// 警示阈值边界:用 `>=`(达到 20K 即触发),19_999 不会触发警示。
    #[test]
    fn warn_boundary_is_inclusive_at_threshold() {
        assert_eq!(
            decide_note(20_000, 1, 0, 0, COST_WARN_OUTPUT_THRESHOLD),
            Some(CostNoteKind::Warn),
            "恰达阈值即应警示(>= 语义,见常量注释)"
        );
        assert_ne!(
            decide_note(19_999, 1, 0, 0, COST_WARN_OUTPUT_THRESHOLD),
            Some(CostNoteKind::Warn),
            "阈值之下不触发警示"
        );
    }

    /// 普通提示基线:output ≥ 5K 或工具往返 ≥ 3 次(且 output > 0)。
    #[test]
    fn hint_fires_on_output_threshold_or_three_tool_calls() {
        // 2 次工具 + 4K output:未达任一基线 → 不提示。
        assert_eq!(
            decide_note(4_000, 2, 0, 0, COST_WARN_OUTPUT_THRESHOLD),
            None
        );
        // 2 次工具 + 5K output:达 output 基线 → 提示。
        assert_eq!(
            decide_note(5_000, 2, 0, 0, COST_WARN_OUTPUT_THRESHOLD),
            Some(CostNoteKind::Hint)
        );
        // 3 次工具 + 小 output:达工具往返基线 → 提示。
        assert_eq!(
            decide_note(600, 3, 0, 0, COST_WARN_OUTPUT_THRESHOLD),
            Some(CostNoteKind::Hint)
        );
        // 3 次工具但 output 为 0(usage 缺失)→ 不提示。
        assert_eq!(decide_note(0, 3, 0, 0, COST_WARN_OUTPUT_THRESHOLD), None);
    }

    /// 警示频率上限:距上次警示 < 10K output 不重复警示;≥ 10K 再警示。
    #[test]
    fn warn_rate_limited_every_10k_output() {
        let mut meter = CostMeter::default();
        meter.add_tool_calls(1);
        meter.record_output(25_000);
        assert!(
            meter.poll_note(COST_WARN_OUTPUT_THRESHOLD).is_some(),
            "25K ≥ 20K 且首次警示应触发"
        );
        // 同状态重复调用不重复注入。
        assert_eq!(meter.poll_note(COST_WARN_OUTPUT_THRESHOLD), None);
        // 29K:距上次(25K)+4K < 10K → 不警示。
        meter.record_output(4_000);
        assert_eq!(meter.poll_note(COST_WARN_OUTPUT_THRESHOLD), None);
        // 36K:距上次(25K)+11K ≥ 10K → 再次警示。
        meter.record_output(7_000);
        assert!(
            meter.poll_note(COST_WARN_OUTPUT_THRESHOLD).is_some(),
            "36K-25K=11K ≥ 10K 应再次警示"
        );
    }

    /// 普通提示频率上限:连续 20 次工具调用(总 output 12K,未触警示)只产生
    /// 4 条提示(第 3/8/13/18 次工具往返),不会逐轮刷屏。
    #[test]
    fn hint_frequency_capped_every_five_tool_calls() {
        let mut meter = CostMeter::default();
        let mut notes = 0usize;
        for _ in 0..20 {
            meter.add_tool_calls(1);
            meter.record_output(600);
            if meter.poll_note(COST_WARN_OUTPUT_THRESHOLD).is_some() {
                notes += 1;
            }
        }
        assert_eq!(
            notes, 4,
            "20 次工具调用应只注记 4 次(每 5 次一条,首个在 3 次工具处)"
        );
    }

    /// 警示优先:同一轮 output 同时超 5K 与 20K 时只注入警示,不追加提示。
    #[test]
    fn warn_takes_priority_over_hint_same_iteration() {
        let mut meter = CostMeter::default();
        meter.add_tool_calls(1);
        meter.record_output(25_000);
        let note = meter.poll_note(COST_WARN_OUTPUT_THRESHOLD).expect("warn");
        assert!(note.contains("[runtime] 成本警示"), "note: {note}");
        assert!(
            note.contains("25000"),
            "警示应携带实际累计 output,note: {note}"
        );
        assert!(!note.contains("成本提示"));
    }

    /// 注记文本渲染:提示携带 output 与工具调用数。
    #[test]
    fn renders_hint_text_with_totals() {
        let note = render_hint(12_000, 7);
        assert!(note.starts_with("[runtime] 成本提示:"), "note: {note}");
        assert!(note.contains("~12000 tokens"), "note: {note}");
        assert!(note.contains("工具调用 7 次"), "note: {note}");
        assert!(note.contains("避免为低价值验证追加调用"), "note: {note}");
    }

    /// env 覆盖警示阈值:合法值生效;非法/低于下限回退默认。测试后恢复原值。
    /// 持共享锁串行化 —— 与集成测试(主循环读同一 env)互斥,防并行污染。
    #[test]
    fn env_overrides_warn_output_threshold() {
        let _guard = COST_ENV_LOCK.lock().expect("env lock");
        let key = COST_WARN_OUTPUT_THRESHOLD_ENV;
        let saved = std::env::var(key).ok();
        std::env::set_var(key, "12345");
        assert_eq!(cost_warn_output_threshold(), 12_345);
        std::env::set_var(key, "abc");
        assert_eq!(cost_warn_output_threshold(), COST_WARN_OUTPUT_THRESHOLD);
        std::env::set_var(key, "1");
        assert_eq!(cost_warn_output_threshold(), COST_WARN_OUTPUT_THRESHOLD);
        match saved {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
