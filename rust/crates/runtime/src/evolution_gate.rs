//! 元认知触发门控(P1)—— 决定"是否要为本次收尾/失败启动元认知总结"。
//!
//! **核心约束**:每次收敛都跑一次 LLM 元认知 = 每 turn 多一次往返,成本不可接受。
//! 故先用**零 LLM 的廉价本地信号**筛选(复用提交 `c871b109` 的"本地预筛短路"模式),
//! 仅命中时才归档提案/启动元认知。
//!
//! 设计见 `docs/2026-10-11-metacognitive-evolution-design.md` §3.2。
//!
//! 本模块只做**纯决策**(输入信号 → 是否触发 + 触发原因),不落盘、不调 LLM,
//! 便于单测与复用。

/// turn 收尾时可用的廉价信号(零 LLM)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnSignals {
    /// 用户打回上一轮答复(`convergence::classify_punchback`)。
    pub punchback: bool,
    /// `LoopDetector` 命中(反复同动作)。
    pub loop_detected: bool,
    /// 连续失败的 tool 调用数。
    pub tool_fail_streak: u32,
    /// turn 累计耗时(秒)。
    pub elapsed_secs: u64,
    /// turn 迭代数。
    pub iterations: usize,
    /// turn 是否硬失败(`record_turn_failed` 路径)。
    pub turn_failed: bool,
    /// 同一 turn 内成功的 tool 调用数(用于判定「只读侦察轮」)。
    ///
    /// 与 [`crate::convergence::project_one`] 的口径配合:调过工具但零进度信号
    /// = 只读侦察轮(全 grep/read,结构性不可能产出 `file_produced`)。
    /// 缺此字段时无法区分「观察不到进度」与「观察到无进度」,`!progress_made`
    /// 对只读轮恒真 —— 257abbd0 引入的条件对这类 turn 等于没加。
    pub tool_calls: usize,
    /// 本 turn 是否产生过**进度信号**(写文件 / 测试通过,见
    /// [`crate::convergence::project_one`])。
    ///
    /// 用于给"耗时/迭代"两轴加必要条件:一个跑了 10 分钟但全程在落盘、
    /// 测试通过的 turn 是**正常长任务**,不是框架缺陷。缺此字段时耗时轴
    /// 退化成"turn 慢就上报",与 `TURN_TIME_SOFT_SECS`(正常收敛阈值)撞在
    /// 同一量级,实测把正常长 turn 100% 判成缺陷(4/4 条积压提案)。
    pub progress_made: bool,
}

/// 门控阈值(可 env 覆盖)。
#[derive(Debug, Clone, Copy)]
pub struct GateThresholds {
    /// tool 连续失败达到该值即触发。
    pub tool_fail_streak: u32,
    /// 墙钟耗时达到该值即触发(秒)。
    pub elapsed_secs: u64,
    /// 迭代数达到该值即触发。
    pub iterations: usize,
}

impl Default for GateThresholds {
    fn default() -> Self {
        Self {
            tool_fail_streak: 5,
            // 与 `TURN_TIME_SOFT_SECS` 默认一致:已到墙钟软阈值即值得复盘。
            elapsed_secs: 600,
            // 与 `SOFT_MAX_ITERATIONS` 默认一致。
            iterations: 64,
        }
    }
}

/// 从环境读取阈值(`CLAW_GATE_TOOL_FAILS` / `CLAW_GATE_ELAPSED_SECS` / `CLAW_GATE_ITERATIONS`)。
#[must_use]
pub fn thresholds_from_env() -> GateThresholds {
    let d = GateThresholds::default();
    let parse = |k: &str, dv: u64| -> u64 {
        std::env::var(k)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(dv)
    };
    GateThresholds {
        tool_fail_streak: parse("CLAW_GATE_TOOL_FAILS", d.tool_fail_streak as u64) as u32,
        elapsed_secs: parse("CLAW_GATE_ELAPSED_SECS", d.elapsed_secs),
        iterations: parse("CLAW_GATE_ITERATIONS", d.iterations as u64) as usize,
    }
}

/// 门控决策:是否触发元认知 + 触发原因(`None` = 不触发)。
///
/// 原则:**任一廉价信号命中即触发**。宁可偶尔多复盘一次,也不漏掉真实框架缺陷
/// —— 因为一次 LLM 往返的成本远低于"框架缺陷长期未被发现"的代价。
/// 但只有**廉价信号命中**才付费,故绝大多数正常收尾(无信号)零额外开销。
#[must_use]
pub fn should_trigger_metacognition(
    signals: &TurnSignals,
    th: &GateThresholds,
) -> Option<&'static str> {
    if signals.turn_failed {
        return Some("task_failed");
    }
    if signals.punchback {
        return Some("punchback");
    }
    if signals.loop_detected {
        return Some("loop_detected");
    }
    if signals.tool_fail_streak >= th.tool_fail_streak {
        return Some("tool_fail_streak");
    }
    // 软阈值两轴(耗时/迭代)必须叠加「本 turn 全程无进度信号」这个必要条件。
    //
    // 缺陷(实测):两轴原本是独立充分条件,而耗时轴阈值恰等于
    // `conversation::TURN_TIME_SOFT_SECS`(600s)——**正常收敛护栏本身的量级**。
    // 于是"turn 长到需要收敛提醒"这一设计内行为,被直接判定为框架缺陷:
    // inbox 4/4 条积压提案全为 `soft_threshold`,迭代 28/50/49/51 均低于迭代
    // 阈值 64,即全部由耗时轴单轴触发;其中 elapsed=638s 仅超出阈值 38s。
    //
    // 语义上「turn 慢」不等于「框架有问题」:一个跑了 10 分钟但持续落盘、
    // 测试通过的长任务是**正常长任务**(与 `CONVERGENCE_MIN_STAGNANT_ITERS`
    // 的既有修正同源:进度信号存在时不该催收尾)。慢 **且** 全程无任何
    // 进度信号,才是真 runaway/空转的证据。故此处要求 `!progress_made`。
    if signals.progress_made {
        return None;
    }
    // 必要条件补齐后**仍有**同源提案持续产生,根因在下一层(实测):
    // `progress_made` 只认「写文件 / 测试通过」两类信号,而**只读侦察轮**
    // (全 grep/read,零写入)结构上不可能产出这两类信号 —— 对它而言
    // `!progress_made` 恒真,上面那条必要条件等于没加。
    // 于是合法的大规模只读调研(多渠道检索、大量文件阅读)被按「写文件轮」
    // 的门槛判定:实测 `1792613485725` iterations=76 elapsed=452s **耗时未达
    // 600s** 却被迭代轴 76≥64 击中 —— 两轴同病,只修一轴不够。
    //
    // 修正与 [`crate::convergence::soft_thresholds`] 同源(BUG:那里已确立
    // 「用写文件轮的阈值判定只读轮不成立」并放宽 token 阈值 15K/25K→40K/60K):
    // 只读侦察轮两轴**对称**放宽同等倍率,既保留对真 runaway 的拦截
    // (放宽后仍有有限上限),又不把正常调研判成框架缺陷。
    let (elapsed_bar, iter_bar) = effective_soft_bars(signals, th);
    if elapsed_bar > 0 && signals.elapsed_secs >= elapsed_bar {
        return Some("soft_threshold");
    }
    if iter_bar > 0 && signals.iterations >= iter_bar {
        return Some("soft_threshold");
    }
    None
}

/// 只读侦察轮阈值放宽倍率(`8/3` ≈ 2.67x)。
///
/// 取值依据:[`crate::convergence::soft_thresholds`] 对只读轮的 token 阈值
/// 放宽为 15K/25K → 40K/60K(2.4x / 2.7x),此处沿用同一放宽量级,
/// 避免两处对「只读轮该用多宽的门槛」各自定标准。
pub const READONLY_WIDEN_NUM: u64 = 8;
/// 见 [`READONLY_WIDEN_NUM`]。
pub const READONLY_WIDEN_DEN: u64 = 3;

/// 本 turn 是否为**只读侦察轮**:调过工具,但从未产生进度信号。
///
/// 纯推理轮(一次工具都没调)不算 —— 它没有「只读」这个解释,保留按量催缴。
#[must_use]
pub fn is_readonly_survey_turn(signals: &TurnSignals) -> bool {
    signals.tool_calls > 0 && !signals.progress_made
}

/// 取本次 turn 实际生效的软阈值 `(elapsed_secs, iterations)`。
///
/// 非只读轮用原值;只读侦察轮按 [`READONLY_WIDEN_NUM`] 放宽。
/// 返回类型沿用 `GateThresholds` 各轴原始类型(耗时 `u64` / 迭代 `usize`),
/// `0` 表示该轴被禁用,放宽后仍为 `0`(禁用语义不被放大破坏)。
#[must_use]
fn effective_soft_bars(signals: &TurnSignals, th: &GateThresholds) -> (u64, usize) {
    if !is_readonly_survey_turn(signals) {
        return (th.elapsed_secs, th.iterations);
    }
    let widen_u64 = |v: u64| v.saturating_mul(READONLY_WIDEN_NUM) / READONLY_WIDEN_DEN;
    let widen_usize = |v: usize| {
        (v as u64).saturating_mul(READONLY_WIDEN_NUM) as usize / READONLY_WIDEN_DEN as usize
    };
    (widen_u64(th.elapsed_secs), widen_usize(th.iterations))
}

/// 从 turn 的工具调用序列派生门控信号:
/// `(progress_made, tool_fail_streak, tool_calls)`。
///
/// - `progress_made`:任一次非失败的写文件/测试通过(`convergence::project_one`
///   产出信号)即为 `true` —— 与收敛催缴用的是**同一套**进度判据,避免两处
///   口径漂移。
/// - `tool_fail_streak`:**尾部连续**失败数。仅尾部连续才有意义:中间失败
///   后来恢复了说明是偶发,不是"反复撞同一堵墙"。
/// - `tool_calls`:本次 turn 的工具结果总数,用于识别只读侦察轮
///   (见 [`is_readonly_survey_turn`])。`progress_made` 为假有两种完全不同的
///   成因 —— 「只读轮根本不可能产出写文件信号」与「写了但失败/没写」;
///   没有调用计数就无法区分,会把前者误判成空转。
///
/// 纯函数,零 IO,便于单测。入参为 `(tool_name, is_error, output)` 序列。
#[must_use]
pub fn derive_turn_signals<'a, I>(results: I) -> (bool, u32, usize)
where
    I: IntoIterator<Item = (&'a str, bool, &'a str)>,
{
    let mut progress_made = false;
    let mut streak: u32 = 0;
    let mut tool_calls: usize = 0;
    for (tool_name, is_error, output) in results {
        tool_calls += 1;
        if !progress_made
            && !crate::convergence::project_one(crate::convergence::ToolOutcome {
                tool_name,
                is_error,
                output,
            })
            .is_empty()
        {
            progress_made = true;
        }
        if is_error {
            streak = streak.saturating_add(1);
        } else {
            streak = 0;
        }
    }
    (progress_made, streak, tool_calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn th() -> GateThresholds {
        GateThresholds::default()
    }

    #[test]
    fn no_signal_does_not_trigger() {
        let s = TurnSignals {
            elapsed_secs: 30,
            iterations: 5,
            ..Default::default()
        };
        assert_eq!(should_trigger_metacognition(&s, &th()), None);
    }

    #[test]
    fn turn_failed_has_highest_priority() {
        let s = TurnSignals {
            turn_failed: true,
            punchback: true,
            loop_detected: true,
            ..Default::default()
        };
        assert_eq!(should_trigger_metacognition(&s, &th()), Some("task_failed"));
    }

    #[test]
    fn each_cheap_signal_triggers() {
        let base = TurnSignals::default();
        assert_eq!(
            should_trigger_metacognition(
                &TurnSignals {
                    punchback: true,
                    ..base
                },
                &th()
            ),
            Some("punchback")
        );
        assert_eq!(
            should_trigger_metacognition(
                &TurnSignals {
                    loop_detected: true,
                    ..base
                },
                &th()
            ),
            Some("loop_detected")
        );
        assert_eq!(
            should_trigger_metacognition(
                &TurnSignals {
                    tool_fail_streak: 5,
                    ..base
                },
                &th()
            ),
            Some("tool_fail_streak")
        );
        assert_eq!(
            should_trigger_metacognition(
                &TurnSignals {
                    elapsed_secs: 600,
                    ..base
                },
                &th()
            ),
            Some("soft_threshold")
        );
        assert_eq!(
            should_trigger_metacognition(
                &TurnSignals {
                    iterations: 64,
                    ..base
                },
                &th()
            ),
            Some("soft_threshold")
        );
    }

    #[test]
    fn below_threshold_does_not_trigger() {
        let s = TurnSignals {
            tool_fail_streak: 4,
            elapsed_secs: 599,
            iterations: 63,
            ..Default::default()
        };
        assert_eq!(should_trigger_metacognition(&s, &th()), None);
    }

    #[test]
    fn zero_threshold_disables_that_axis() {
        let th = GateThresholds {
            elapsed_secs: 0,
            iterations: 0,
            ..GateThresholds::default()
        };
        let s = TurnSignals {
            elapsed_secs: 99999,
            iterations: 9999,
            ..Default::default()
        };
        assert_eq!(should_trigger_metacognition(&s, &th), None);
    }

    /// 回归(实测缺陷):慢但**全程有进度**的长 turn 是正常任务,不该立案。
    ///
    /// 证据:inbox 4/4 条积压提案全为 `soft_threshold`,iterations=28/50/49/51
    /// 均 < 迭代阈值 64 → 全部由耗时轴单轴触发(elapsed 638/2350/1061/605s)。
    /// 修复前 `elapsed >= 600` 单独即判框架缺陷,把"turn 长到该提醒收敛"的
    /// 设计内行为误判成缺陷。
    #[test]
    fn slow_turn_with_progress_does_not_trigger() {
        let s = TurnSignals {
            elapsed_secs: 2350,
            iterations: 50,
            progress_made: true,
            ..Default::default()
        };
        assert_eq!(should_trigger_metacognition(&s, &th()), None);
    }

    /// 慢 **且** 全程无进度信号 = 真空转,仍须立案(修复不能把真 runaway 也放过)。
    #[test]
    fn slow_turn_without_progress_still_triggers() {
        let s = TurnSignals {
            elapsed_secs: 638,
            iterations: 28,
            progress_made: false,
            ..Default::default()
        };
        assert_eq!(
            should_trigger_metacognition(&s, &th()),
            Some("soft_threshold")
        );
    }

    /// 迭代轴同受"无进度"约束(两轴共用 `stalled` 判据,防止只修了一轴)。
    #[test]
    fn iteration_axis_also_requires_no_progress() {
        let s = TurnSignals {
            iterations: 64,
            progress_made: true,
            ..Default::default()
        };
        assert_eq!(should_trigger_metacognition(&s, &th()), None);
    }

    /// `derive_turn_signals`:写文件成功 ⇒ 有进度;尾部连续失败 ⇒ streak。
    #[test]
    fn derive_signals_from_tool_results() {
        // 写文件成功 → progress_made;其后两次失败 → 尾部 streak=2。
        let (progress, streak, calls) = derive_turn_signals(vec![
            ("read_file", false, "content"),
            ("write_file", false, "written"),
            ("bash", true, "boom"),
            ("bash", true, "boom again"),
        ]);
        assert!(progress);
        assert_eq!(streak, 2);
        assert_eq!(calls, 4);

        // 失败后又成功 → streak 归零(偶发失败不是"反复撞墙")。
        let (progress, streak, calls) = derive_turn_signals(vec![
            ("grep_search", true, "no match"),
            ("grep_search", false, "matched"),
        ]);
        assert!(!progress, "只读检索不产生进度信号");
        assert_eq!(streak, 0);
        assert_eq!(calls, 2);

        // 空序列 → 无进度、无失败、零调用。
        let (progress, streak, calls) = derive_turn_signals(Vec::new());
        assert!(!progress);
        assert_eq!(streak, 0);
        assert_eq!(calls, 0);
    }

    /// `derive_turn_signals`:测试通过也算进度(与收敛催缴同一套判据)。
    #[test]
    fn derive_signals_counts_test_success_as_progress() {
        let (progress, streak, calls) =
            derive_turn_signals(vec![("bash", false, "running 11 tests\nok. 11 passed")]);
        assert!(progress);
        assert_eq!(streak, 0);
        assert_eq!(calls, 1);
    }

    /// 回归(实测缺陷,257abbd0 修复后仍持续产生同源提案):只读侦察轮
    /// (`tool_calls>0 && !progress_made`)对 `progress_made` 判据是**结构性免疫**的
    /// —— 只读工具永远不产出 `file_produced`/`test_success`,故
    /// 「慢且无进度」这条必要条件对它恒成立,等于没加。
    ///
    /// 证据:提交 257abbd0(修复「turn 慢即上报」)之后 inbox 仍新增同源提案
    /// `1792613485725` iterations=76 elapsed=452s —— **耗时未达 600s**,
    /// 由迭代轴 76≥64 单轴击中,正是只读轮;`1792608694118` iterations=28
    /// elapsed=930s 同理。两轴同病,必须对称放宽。
    #[test]
    fn readonly_survey_turn_is_not_framework_defect() {
        // 只读轮:76 次迭代、452s,零写入 —— 原实现判 soft_threshold(误报)。
        let survey = TurnSignals {
            tool_calls: 76,
            elapsed_secs: 452,
            iterations: 76,
            progress_made: false,
            ..Default::default()
        };
        assert!(is_readonly_survey_turn(&survey));
        assert_eq!(
            should_trigger_metacognition(&survey, &th()),
            None,
            "只读侦察轮是正常调研,不是框架缺陷"
        );
    }

    /// 放宽不是取消:只读轮超过放宽后的上限(**真** runaway)仍须立案,
    /// 否则修复会把真缺陷放过。
    #[test]
    fn readonly_survey_turn_still_triggers_beyond_widened_bar() {
        // 迭代轴放宽后 = 64 * 8/3 = 170;171 次只读迭代无产出 = 真空转。
        let runaway = TurnSignals {
            tool_calls: 200,
            iterations: 171,
            elapsed_secs: 100,
            progress_made: false,
            ..Default::default()
        };
        assert_eq!(
            should_trigger_metacognition(&runaway, &th()),
            Some("soft_threshold")
        );

        // 耗时轴放宽后 = 600 * 8/3 = 1600;1601s 无任何产出 = 真空转。
        let runaway = TurnSignals {
            tool_calls: 5,
            iterations: 5,
            elapsed_secs: 1601,
            progress_made: false,
            ..Default::default()
        };
        assert_eq!(
            should_trigger_metacognition(&runaway, &th()),
            Some("soft_threshold")
        );
    }

    /// 放宽只对**只读侦察轮**生效:纯推理轮(零 tool 调用)没有「只读」这个
    /// 解释,仍按原阈值判定,避免借放宽漏掉真缺陷。
    #[test]
    fn pure_reasoning_turn_keeps_original_bar() {
        let s = TurnSignals {
            tool_calls: 0,
            iterations: 64,
            progress_made: false,
            ..Default::default()
        };
        assert!(!is_readonly_survey_turn(&s));
        assert_eq!(
            should_trigger_metacognition(&s, &th()),
            Some("soft_threshold")
        );
    }

    /// 禁用语义不被放宽破坏:`0` 轴放宽后仍须为 `0`(不因乘倍率变正)。
    #[test]
    fn widening_does_not_re_enable_disabled_axis() {
        let th = GateThresholds {
            elapsed_secs: 0,
            iterations: 0,
            ..GateThresholds::default()
        };
        let s = TurnSignals {
            tool_calls: 500,
            elapsed_secs: 99999,
            iterations: 9999,
            progress_made: false,
            ..Default::default()
        };
        assert_eq!(should_trigger_metacognition(&s, &th), None);
    }
}
