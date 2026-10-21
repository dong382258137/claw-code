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
    let stalled = !signals.progress_made;
    if stalled && th.elapsed_secs > 0 && signals.elapsed_secs >= th.elapsed_secs {
        return Some("soft_threshold");
    }
    if stalled && th.iterations > 0 && signals.iterations >= th.iterations {
        return Some("soft_threshold");
    }
    None
}

/// 从 turn 的工具调用序列派生两个门控信号:`(progress_made, tool_fail_streak)`。
///
/// - `progress_made`:任一次非失败的写文件/测试通过(`convergence::project_one`
///   产出信号)即为 `true` —— 与收敛催缴用的是**同一套**进度判据,避免两处
///   口径漂移。
/// - `tool_fail_streak`:**尾部连续**失败数。仅尾部连续才有意义:中间失败
///   后来恢复了说明是偶发,不是"反复撞同一堵墙"。
///
/// 纯函数,零 IO,便于单测。入参为 `(tool_name, is_error, output)` 序列。
#[must_use]
pub fn derive_turn_signals<'a, I>(results: I) -> (bool, u32)
where
    I: IntoIterator<Item = (&'a str, bool, &'a str)>,
{
    let mut progress_made = false;
    let mut streak: u32 = 0;
    for (tool_name, is_error, output) in results {
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
    (progress_made, streak)
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
        let (progress, streak) = derive_turn_signals(vec![
            ("read_file", false, "content"),
            ("write_file", false, "written"),
            ("bash", true, "boom"),
            ("bash", true, "boom again"),
        ]);
        assert!(progress);
        assert_eq!(streak, 2);

        // 失败后又成功 → streak 归零(偶发失败不是"反复撞墙")。
        let (progress, streak) = derive_turn_signals(vec![
            ("grep_search", true, "no match"),
            ("grep_search", false, "matched"),
        ]);
        assert!(!progress, "只读检索不产生进度信号");
        assert_eq!(streak, 0);

        // 空序列 → 无进度、无失败。
        let (progress, streak) = derive_turn_signals(Vec::new());
        assert!(!progress);
        assert_eq!(streak, 0);
    }

    /// `derive_turn_signals`:测试通过也算进度(与收敛催缴同一套判据)。
    #[test]
    fn derive_signals_counts_test_success_as_progress() {
        let (progress, streak) =
            derive_turn_signals(vec![("bash", false, "running 11 tests\nok. 11 passed")]);
        assert!(progress);
        assert_eq!(streak, 0);
    }
}
