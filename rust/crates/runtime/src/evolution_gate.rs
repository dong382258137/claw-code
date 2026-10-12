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
    if th.elapsed_secs > 0 && signals.elapsed_secs >= th.elapsed_secs {
        return Some("soft_threshold");
    }
    if th.iterations > 0 && signals.iterations >= th.iterations {
        return Some("soft_threshold");
    }
    None
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
}
