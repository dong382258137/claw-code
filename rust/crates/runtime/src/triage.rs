//! 轮次分诊档位类型 + 单向升级检测(轮次感知上下文编译器 Phase 1 的
//! 类型与协议部分)。
//!
//! # 定位(2026-09-04 精简后)
//! 本模块现仅承担「档位类型 + 单向升级检测」两件事:
//! - [`TriageTier`]:轮次分诊档位 —— 随请求下发(`ApiRequest.triage_tier`),
//!   CLI 层据此裁剪 tools(Trivial 档 = 核心工具集);
//! - [`detect_upgrade_request`]:从 assistant 文本检测模型显式请求加载
//!   完整上下文/能力层的协议化标记(`[LOAD_FULL_CONTEXT]`)+ 少量自然
//!   语言兜底。会话级 `upgrade_requested` 标志的锁存由调用方负责
//!   (`ConversationRuntime::triage_upgrade_requested`),本模块不持有可变状态。
//!
//! # 纯规则分诊已移除(2026-09-04)
//! 原 `triage_turn` 纯规则分诊(词表近似:长度上限 / 工程动词 / 状态特征词
//! 判定 Trivial)已删除:真实回放命中率 ~1%(102 条仅 1 条),判定无效。
//! 现档位完全由 LLM 分诊承担(见 [`crate::triage_llm`]);LLM 分诊失败 /
//! 超时 / 被禁用(`CLAW_TRIAGE_MODE=off`)/ `with_model` 不可用时,
//! 调用方直接**跳过分诊**返回 [`TriageTier::Standard`](全量注入,
//! 与无分诊等价),不再降级到任何纯规则兜底。
//!
//! 设计原则(保守优先):宁可不裁剪,也不误伤正常工程轮次 —— 因此一切
//! 无法可靠判定 Trivial 的情况都走 Standard。

/// 单向升级协议标记(主要协议入口):assistant 文本中出现该标记即视为
/// 显式请求加载完整上下文/能力层。
pub const UPGRADE_MARKER: &str = "[LOAD_FULL_CONTEXT]";

/// 单向升级自然语言兜底词表:在转小写后的 assistant 文本上做子串匹配
/// (英文部分大小写不敏感,如「需要多Agent」)。
const UPGRADE_PHRASES: &[&str] = &[
    "升级为完整上下文",
    "加载完整上下文",
    "加载能力层",
    "需要多agent",
    "需要多 agent",
];

/// 轮次分诊档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageTier {
    /// 状态/问答类简单轮次:只注入不变核+状态层+核心工具。
    Trivial,
    /// 标准轮次:全量注入(与现状等价)。
    Standard,
}

/// 从 assistant 文本检测模型显式请求加载能力层(协议化文本标记)。
///
/// 匹配顺序:
/// 1. 协议标记 [`UPGRADE_MARKER`](`[LOAD_FULL_CONTEXT]`,主要协议入口,
///    在原文上做精确子串匹配,区分大小写);
/// 2. 自然语言兜底(见 `UPGRADE_PHRASES`,如「升级为完整上下文」
///    「需要加载能力层」「需要多Agent」,英文大小写不敏感)。
///
/// 本函数只做检测,不持有状态;会话级 `upgrade_requested` 标志的锁存
/// 由调用方(`ConversationRuntime::triage_upgrade_requested`)负责。
pub fn detect_upgrade_request(assistant_text: &str) -> bool {
    // 协议标记:精确匹配原文本(协议入口不做大小写折叠)。
    if assistant_text.contains(UPGRADE_MARKER) {
        return true;
    }
    // 自然语言兜底:在转小写后的文本上做子串匹配。
    let lowered = assistant_text.to_lowercase();
    UPGRADE_PHRASES
        .iter()
        .any(|phrase| lowered.contains(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_upgrade_request ──

    /// 协议标记 `[LOAD_FULL_CONTEXT]` → true(主要协议入口)。
    #[test]
    fn upgrade_marker_detected() {
        assert!(detect_upgrade_request("[LOAD_FULL_CONTEXT]"));
        // 嵌入在正文中的协议标记同样命中。
        assert!(detect_upgrade_request(
            "分析完成。[LOAD_FULL_CONTEXT] 请继续。"
        ));
    }

    /// 自然语言兜底:「请升级为完整上下文」等 → true。
    #[test]
    fn upgrade_natural_language_fallback_detected() {
        assert!(detect_upgrade_request("请升级为完整上下文"));
        assert!(detect_upgrade_request("需要加载能力层才能继续"));
        // 英文大小写不敏感:「需要多Agent」→ 命中「需要多agent」。
        assert!(detect_upgrade_request("这个任务需要多Agent协作"));
    }

    /// 普通文本 → false(不误触发单向升级)。
    #[test]
    fn ordinary_text_is_not_upgrade_request() {
        assert!(!detect_upgrade_request("任务已完成,请查看结果"));
        // 无方括号/大小写不符的伪标记不应命中(协议入口区分大小写)。
        assert!(!detect_upgrade_request("load_full_context"));
        assert!(!detect_upgrade_request(""));
    }
}
