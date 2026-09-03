//! 轮次分诊门(Triage Gate)— 轮次感知上下文编译器(Phase 1)的第一环。
//!
//! # LLM 分诊降级路径(2026-09-04 升级后定位)
//! 自入口全量 LLM 分诊上线(见 [`crate::triage_llm`])起,本模块的纯规则分诊
//! **降级为 fallback**:LLM 调用被禁用(`CLAW_TRIAGE_MODE=rule`)/ 失败 /
//! `with_model` 不可用等任何无法走 LLM 的路径,回退到这里按原语义判定。
//! 本模块语义与实现保持稳定(不改动);升级只发生在调用方
//! ([`crate::conversation::ConversationRuntime::triage_turn_llm`] 优先走 LLM)。
//!
//! 设计动机:状态/问答类简单轮次(如「上次任务执行到哪里了」)并不需要
//! 完整的工具目录与能力层注入。在 prompt 组装之前,用一个纯确定性、零 LLM
//! 的分诊门把用户轮次划分为两档:
//! - Trivial 档([`TriageTier::Trivial`]):状态/问答类简单轮次,
//!   只注入不变核+状态层+核心工具;
//! - Standard 档([`TriageTier::Standard`]):标准轮次,全量注入(与现状等价)。
//!
//! 设计原则(保守优先):只在「高置信简单轮次」输出 Trivial;空输入、超长、
//! 含工程动词、无状态特征等任何不确定情况一律 Standard —— 宁可不裁剪,
//! 也不误伤正常工程轮次。
//!
//! 规则优先级(高 → 低):单向升级 > 超长 > 工程动词 > 状态特征,
//! 详见 [`triage_turn`] 的实现注释。
//!
//! 单向升级通道:[`detect_upgrade_request`] 从 assistant 文本检测模型显式
//! 请求加载能力层的协议化文本标记(`[LOAD_FULL_CONTEXT]`)+ 少量自然语言
//! 兜底。会话级 `upgrade_requested` 标志的锁存由调用方负责(Task 3 接入
//! `run_turn`),本模块不持有可变状态。

/// Trivial 档位的长度上限(按字符数计,中文一字一字符)。
///
/// 阈值含边界:恰好 `TRIVIAL_MAX_CHARS` 个字符的输入仍可判 Trivial
/// (实现用 `chars().count() > TRIVIAL_MAX_CHARS` 判「超长」,
/// 即严格大于该值才判超长 → Standard)。
pub const TRIVIAL_MAX_CHARS: usize = 120;

/// 工程动词词表(中文):命中任一 → Standard(轮次含明确工程任务意图)。
/// 按子串匹配,不需要分词。后续扩展直接追加词条。
const ENGINEERING_VERBS_ZH: &[&str] = &[
    "改",
    "修改",
    "修复",
    "实现",
    "重构",
    "编写",
    "新增",
    "删除",
    "添加",
    "部署",
    "调试",
    "优化",
    "写一个",
    "创建",
    "搭建",
    "迁移",
];

/// 工程动词词表(英文):匹配前统一转小写(大小写不敏感),按子串匹配。
const ENGINEERING_VERBS_EN: &[&str] = &["refactor", "fix", "implement", "build", "create"];

/// 状态/问答特征词表(中文):短文本(未超长、无工程动词)命中任一 → Trivial。
/// 按子串匹配,不需要分词。后续扩展直接追加词条。
const STATUS_CUES_ZH: &[&str] = &[
    "到哪了",
    "哪里了",
    "什么状态",
    "进展",
    "进度",
    "上次",
    "上次任务",
    "刚刚",
    "现在是什么",
    "是多少",
    "多少",
    "什么情况",
    "怎么样了",
    "完成了吗",
    "好了吗",
    "还在跑吗",
    "上一次",
    "最近一次",
];

/// 状态/问答特征词表(英文):匹配前统一转小写(大小写不敏感),按子串匹配。
const STATUS_CUES_EN: &[&str] = &["history", "status", "progress"];

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

/// 分诊上下文(刻意保持最小:仅当前会话是否已发生单向升级)。
///
/// 生命周期由调用方(Task 3)维护:每次 [`detect_upgrade_request`] 命中后
/// 把 `upgrade_requested` 置 true 并在会话内锁存(单向,不可回退)。
pub struct TriageContext {
    /// 会话级单向升级标志:一旦 true,恒返回 [`TriageTier::Standard`]。
    pub upgrade_requested: bool,
}

/// 判定用户输入的档位。
///
/// 规则(按优先级从高到低,全部为纯确定性规则,零 LLM):
/// 1. `ctx.upgrade_requested == true` → 恒 Standard(单向升级,本会话锁定);
/// 2. 输入为空/纯空白 → Standard(保守默认);
/// 3. 长度超过 [`TRIVIAL_MAX_CHARS`] 字符 → Standard(超长直接 Standard,
///    阈值含边界:恰好 120 字符仍可判 Trivial);
/// 4. 含工程动词(见 `ENGINEERING_VERBS_ZH`/`ENGINEERING_VERBS_EN`)→ Standard;
/// 5. 含状态/问答特征词(见 `STATUS_CUES_ZH`/`STATUS_CUES_EN`)→ Trivial;
/// 6. 其余(既无状态特征也无工程动词的短文本)→ Standard(保守默认,
///    宁可不裁剪也不误伤)。
pub fn triage_turn(user_text: &str, ctx: &TriageContext) -> TriageTier {
    // 优先级 1:单向升级 —— 会话内一旦请求过完整上下文,恒返回 Standard。
    if ctx.upgrade_requested {
        return TriageTier::Standard;
    }
    // 优先级 2:空/纯空白 → 保守默认 Standard。
    let trimmed = user_text.trim();
    if trimmed.is_empty() {
        return TriageTier::Standard;
    }
    // 优先级 3:超长(按字符数计,中文一字一字符)→ Standard。
    // 注意阈值含边界:用 `>` 而非 `>=`,恰好 120 字符不算超长。
    if trimmed.chars().count() > TRIVIAL_MAX_CHARS {
        return TriageTier::Standard;
    }
    let lowered = trimmed.to_lowercase();
    // 优先级 4:工程动词(优先于状态特征):如「修复任务进展怎么样了」
    // 同时命中动词与状态词,仍按 Standard 处理,避免误裁剪工程轮次。
    if ENGINEERING_VERBS_ZH.iter().any(|kw| trimmed.contains(kw))
        || ENGINEERING_VERBS_EN.iter().any(|kw| lowered.contains(kw))
    {
        return TriageTier::Standard;
    }
    // 优先级 5:状态/问答特征词 → Trivial(唯一产出 Trivial 的分支)。
    if STATUS_CUES_ZH.iter().any(|kw| trimmed.contains(kw))
        || STATUS_CUES_EN.iter().any(|kw| lowered.contains(kw))
    {
        return TriageTier::Trivial;
    }
    // 优先级 6:兜底 —— 既无状态特征也无工程动词 → 保守默认 Standard。
    TriageTier::Standard
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
/// 由调用方(Task 3)负责。
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

    /// 未升级的默认分诊上下文。
    fn fresh_ctx() -> TriageContext {
        TriageContext {
            upgrade_requested: false,
        }
    }

    /// 已发生单向升级的分诊上下文。
    fn upgraded_ctx() -> TriageContext {
        TriageContext {
            upgrade_requested: true,
        }
    }

    // ── triage_turn:基准用例(必须过) ──

    /// 基准用例:纯状态查询 → Trivial。
    #[test]
    fn baseline_status_query_is_trivial() {
        assert_eq!(
            triage_turn("上次任务执行到哪里了", &fresh_ctx()),
            TriageTier::Trivial
        );
    }

    /// 带标点的状态查询 → Trivial。
    #[test]
    fn status_query_with_punctuation_is_trivial() {
        assert_eq!(
            triage_turn("上次任务执行到哪里了？", &fresh_ctx()),
            TriageTier::Trivial
        );
    }

    /// 带 lane 前缀的进展查询 → Trivial。
    #[test]
    fn lane_progress_query_is_trivial() {
        assert_eq!(
            triage_turn("#2 回测进展怎么样了", &fresh_ctx()),
            TriageTier::Trivial
        );
    }

    // ── triage_turn:Standard 用例 ──

    /// 工程动词「修复」+ 技术细节 → Standard。
    #[test]
    fn engineering_fix_request_is_standard() {
        assert_eq!(
            triage_turn("修复 divergence.py 的 O(N²) 性能问题", &fresh_ctx()),
            TriageTier::Standard
        );
    }

    /// 工程动词「写一个」→ Standard。
    #[test]
    fn engineering_write_request_is_standard() {
        assert_eq!(
            triage_turn("帮我写一个新的回测脚本", &fresh_ctx()),
            TriageTier::Standard
        );
    }

    /// 空输入/纯空白 → Standard(保守默认)。
    #[test]
    fn empty_or_whitespace_is_standard() {
        assert_eq!(triage_turn("", &fresh_ctx()), TriageTier::Standard);
        assert_eq!(triage_turn("   ", &fresh_ctx()), TriageTier::Standard);
        assert_eq!(triage_turn("\n\t  \n", &fresh_ctx()), TriageTier::Standard);
    }

    /// 超长(>120 字符)的状态类文本 → Standard(超长规则优先于状态特征)。
    #[test]
    fn overlong_status_text_is_standard() {
        // 构造 > TRIVIAL_MAX_CHARS 字符、以状态特征词结尾的文本。
        let text = format!("{}怎么样了", "甲".repeat(TRIVIAL_MAX_CHARS));
        assert!(text.chars().count() > TRIVIAL_MAX_CHARS);
        assert_eq!(triage_turn(&text, &fresh_ctx()), TriageTier::Standard);
    }

    /// 边界:恰好 120 字符的状态类文本 → Trivial。
    /// 阈值含边界:实现用 `chars().count() > TRIVIAL_MAX_CHARS` 判超长
    /// (即 `<= 120` 都不算超长),因此恰好 120 字符仍可判 Trivial。
    #[test]
    fn exactly_max_chars_status_text_is_trivial() {
        assert_eq!(TRIVIAL_MAX_CHARS, 120);
        let cue = "怎么样了"; // 4 字符状态特征词
        let text = format!(
            "{}{}",
            "甲".repeat(TRIVIAL_MAX_CHARS - cue.chars().count()),
            cue
        );
        assert_eq!(text.chars().count(), TRIVIAL_MAX_CHARS); // 恰好 120
        assert_eq!(triage_turn(&text, &fresh_ctx()), TriageTier::Trivial);
    }

    /// 优先级:工程动词 > 状态特征(同时命中 → Standard)。
    #[test]
    fn engineering_verb_beats_status_cue() {
        // 同时含「修复」(动词)与「进展/怎么样了」(状态词)→ Standard。
        assert_eq!(
            triage_turn("修复任务进展怎么样了", &fresh_ctx()),
            TriageTier::Standard
        );
    }

    /// 单向升级锁存:upgrade_requested=true 时任意输入恒 Standard(含基准用例)。
    #[test]
    fn upgrade_requested_locks_standard() {
        // 基准用例在升级后也必须是 Standard。
        assert_eq!(
            triage_turn("上次任务执行到哪里了", &upgraded_ctx()),
            TriageTier::Standard
        );
        // 其他状态查询 / 工程轮次 / 空输入 同样恒 Standard。
        assert_eq!(
            triage_turn("#2 回测进展怎么样了", &upgraded_ctx()),
            TriageTier::Standard
        );
        assert_eq!(
            triage_turn("帮我写一个新的回测脚本", &upgraded_ctx()),
            TriageTier::Standard
        );
        assert_eq!(triage_turn("", &upgraded_ctx()), TriageTier::Standard);
    }

    /// 英文状态特征词大小写不敏感 → Trivial。
    #[test]
    fn english_status_cue_case_insensitive_is_trivial() {
        assert_eq!(
            triage_turn("What's the STATUS now?", &fresh_ctx()),
            TriageTier::Trivial
        );
    }

    /// 英文工程动词大小写不敏感 → Standard。
    #[test]
    fn english_engineering_verb_case_insensitive_is_standard() {
        assert_eq!(
            triage_turn("please FIX the bug", &fresh_ctx()),
            TriageTier::Standard
        );
    }

    /// 无任何特征的短文本 → Standard(保守默认,宁可不裁剪也不误伤)。
    #[test]
    fn short_text_without_cues_is_standard() {
        assert_eq!(triage_turn("你好", &fresh_ctx()), TriageTier::Standard);
        assert_eq!(triage_turn("在吗", &fresh_ctx()), TriageTier::Standard);
    }

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
