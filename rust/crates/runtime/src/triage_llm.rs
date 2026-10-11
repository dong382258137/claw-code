//! 入口全量 LLM 分诊 —— 「每个用户 turn 在请求构造前,用一次轻量 LLM 调用
//! (deepseek-v4-flash)判断档位」的路由器模式。
//!
//! # 背景(2026-09-04)
//! 原纯规则分诊是词汇近似,命中率低(真实回放 102 条仅 1 条 Trivial),
//! 判定无效,已于同日**移除**(见 [`crate::triage`])。现分诊只走 LLM:
//! - 入口用一次轻量 LLM 调用(deepseek-v4-flash)判断本轮档位;
//! - LLM 失败 / 超时 / 被禁用(`CLAW_TRIAGE_MODE=off`)/ `with_model`
//!   不可用 → **直接跳过分诊**,调用方返回 [`TriageTier::Standard`]
//!   (全量注入,与无分诊等价);不降级到纯规则(已不存在)。
//!
//! 本模块只含纯函数(system prompt / 解析 / 开关),不依赖 `ApiClient` 类型,
//! 全部可离线单测;异步调用接入见
//! [`crate::conversation::ConversationRuntime::triage_turn_llm`]。
//!
//! # 「关闭思考」的实际形态(以代码为准,2026-10-03 更新)
//! - `deepseek-v4-flash` 就是普通模型别名(`resolve_model_alias("flash")` →
//!   `"deepseek-v4-flash"`,见 api::providers),**不存在独立的"无思考"模型名**。
//! - API 层思考开关是 `api::MessageRequest.thinking_mode: Option<bool>`:
//!   `Some(false)` → 请求体注入 `"thinking": {"type": "disabled"}`
//!   (仅 DeepSeek 模型,见 api::providers::openai_compat 的 payload 构造);
//!   `None` → 不发送该字段,走后端默认(DeepSeek V4 默认开思考)。
//! - 分诊请求走 API 端(2026-10-03 起;此前曾路由到网页零 token,被 DeepSeek
//!   官方风控、账号被禁,网页混合分工 `RoutingApiClient` 已停用删除)。
//!   由 [`crate::conversation::ConversationRuntime::triage_turn_llm`] 在
//!   `with_model(TRIAGE_MODEL)` 后显式调用 `set_thinking_mode(Some(false))`
//!   (经 trait 动态分发到 `AnthropicRuntimeClient`,请求体注入
//!   `thinking: disabled`),确保档位判断不走思考、成本与延迟最低。
//! - Trivial 档**主回复**同样关闭思考:`run_turn_async` 判定 `turn_tier` 后
//!   按档位设置主 client 的 thinking_mode(Trivial → Some(false),Standard →
//!   恢复 CLI 基线),见 [`crate::conversation`] 中「按档位自动设置主 client
//!   思考模式」注释。
//! - 分诊解析只取 assistant 文本块 —— 即便模型默认思考,thinking 块也被
//!   忽略,输出不受影响;关闭思考进一步降低该轻量调用的开销。

use crate::triage::TriageTier;

/// 分诊使用的轻量模型:budget 档 flash,关闭思考即可胜任档位判断。
///
/// 该模型名经 [`crate::conversation::ConversationRuntime::triage_turn_llm`]
/// 中的 `api_client.with_model(TRIAGE_MODEL)` 解析为独立 client
/// (production `AnthropicRuntimeClient::with_model` 可用,构造
/// `enable_tools=false` 的轻量子 client)。
pub const TRIAGE_MODEL: &str = "deepseek-v4-flash";

/// 分诊 LLM 的 system prompt(中文,轻量)。
///
/// 内容要点:让模型只输出一个词档位。保守优先(不确定 → standard),
/// 宁可不裁剪也不误伤正常工程轮次。含「档位后果」说明(trivial 会裁剪
/// 能力层教程与长尾工具,只留状态层 + 核心工具)与易混淆边界 few-shot
/// 对比示例,降低「解释现状」与「动手修改」的误判率。全静态文本
/// (单 section),同内容请求可命中 DeepSeek prefix cache。
pub const TRIAGE_LLM_SYSTEM: &str = "\
你是任务分诊器。根据用户输入判断本轮对话应注入哪种上下文档位,只输出一个词:trivial 或 standard。

- trivial:纯状态/问答/简单说明类轮次。注入时会裁剪能力层教程与长尾工具,
  仅保留状态层与核心工具集(读/查文件等),关闭思考模式。
- standard:任何工程任务或不确定的情况。全量注入完整上下文与全部工具。

易混淆边界(重点区分「解释现状」与「动手修改」):
- 「这个报错信息是什么意思」→ trivial(解释现状)
- 「帮我修这个报错」→ standard(修改代码)
- 「上次任务执行到哪里了」→ trivial(读状态)
- 「继续执行上次的任务」→ standard(接续工程)
- 「当前工作目录是什么」→ trivial(状态问答)
- 「检查分析这段代码为什么慢」→ standard(调试分析)

保守优先:不确定一律 standard —— 宁可多花 token,也不把工程轮次裁剪成能力残缺。

只输出一个词(trivial 或 standard),禁止输出任何其他内容、解释、标点或引号。";

/// `CLAW_TRIAGE_MODE` 环境变量名。
///
/// 取值(两态):
/// - 缺失 / `llm` / 其他任何值(默认):LLM 分诊;
/// - `off`(大小写不敏感):跳过分诊 —— 调用方直接返回 Standard 全量注入。
///
/// `rule`(旧纯规则降级)语义已于 2026-09-04 随纯规则分诊一并移除:
/// 该值现在按「非 off」处理,即照常走 LLM 分诊。
pub const TRIAGE_MODE_ENV: &str = "CLAW_TRIAGE_MODE";

/// 串行化「触碰 `CLAW_TRIAGE_MODE` env」的测试。
///
/// 与既有 `COST_ENV_LOCK`(cost_meter.rs)同模式:设置 env 的测试与依赖默认
/// 分诊模式的集成测试并行运行会互相污染,须共享同一把锁。`#[cfg(test)]` 下
/// 定义,conversation.rs 集成测试经 `crate::triage_llm::TRIAGE_ENV_LOCK`
/// 引用(仅测试构建存在)。
#[cfg(test)]
pub(crate) static TRIAGE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// LLM 分诊是否启用(env `CLAW_TRIAGE_MODE` 解析)。
///
/// - 值 `off`(大小写不敏感)→ `false`:跳过分诊 —— 调用方返回
///   [`TriageTier::Standard`](全量注入),不降级纯规则(已移除);
/// - 其余(缺失 / `llm` / 其他值)→ `true`:开启 LLM 分诊(默认)。
pub fn triage_llm_enabled() -> bool {
    !std::env::var(TRIAGE_MODE_ENV).is_ok_and(|v| v.eq_ignore_ascii_case("off"))
}

/// 预筛最小字符数:达到即视为长输入(工程任务描述/粘贴报错/多行需求)。
const PRESCREEN_MIN_CHARS: usize = 24;

/// 预筛用的**明确工程动作词表**。
///
/// 刻意**不含**「执行 / 检查 / 分析」等歧义词 —— 它们同样出现在状态问答里
/// (如 few-shot 的「上次任务执行到哪里了」→ trivial),纳入会削弱预筛精度。
/// 保守起见只收「几乎只可能出现在动手/深读轮次」的词。
const PRESCREEN_ENGINEERING_VERBS: &[&str] = &[
    "修改",
    "修复",
    "实现",
    "重构",
    "新增",
    "添加",
    "删除",
    "移除",
    "优化",
    "部署",
    "编写",
    "重写",
    "迁移",
    "接入",
    "改成",
    "改为",
    "替换",
    "跑一下",
    "edit",
    "fix",
    "implement",
    "refactor",
    "debug",
    "patch",
    "migrate",
    "rename",
];

/// 本地零成本预筛:在触发 LLM 分诊**之前**,用廉价规则识别「确定属 Standard」的
/// 轮次并短路,从而省掉该 turn 的一次完整网络往返。
///
/// # 动机(2026-10-09)
/// [`crate::conversation::ConversationRuntime::triage_turn_llm`] 是**每 turn 一次
/// 完整 LLM 往返**,且位于 `turn_started` 与首次主请求之间 —— 直接落在首轮关键
/// 路径上(实测本仓库单次 provider 往返 8–140s)。而档位裁剪的收益(Trivial 档
/// 省下的长尾工具描述 + 关闭思考)只是一次往返墙钟成本的零头。工程轮次占多数,
/// 本可本地判定,无需为此付费。
///
/// # 安全性质(为什么可以放心短路)
/// 本函数**只输出「Standard / 不判定」,永不输出 Trivial**。因此它最多让一个本可
/// 裁剪的 Trivial 轮次退回全量注入(损失一次 token 优化),绝不会把工程轮次误裁
/// 成 Trivial —— 与分诊一贯的「保守优先:宁可不裁剪,也不误伤工程轮次」一致。
/// 任何不确定的输入返回 `false`,交由既有 LLM 分诊按原语义判定。
///
/// # 判据(任一命中即 Standard)
/// 1. 绝对长度 ≥ [`PRESCREEN_MIN_CHARS`];
/// 2. 含路径分隔符(`/` 或 `\`)—— 涉及具体文件几乎必然要动手或深读;
/// 3. 命中 [`PRESCREEN_ENGINEERING_VERBS`] 中的明确工程动作词。
pub fn prescreen_standard(user_text: &str) -> bool {
    let trimmed = user_text.trim();
    if trimmed.is_empty() {
        return false;
    }
    // 信号 1:长输入。
    if trimmed.chars().count() >= PRESCREEN_MIN_CHARS {
        return true;
    }
    // 信号 2:路径/文件名引用。
    if trimmed.contains('/') || trimmed.contains('\\') {
        return true;
    }
    // 信号 3:明确工程动作动词(ASCII 大小写不敏感)。
    let lowered = trimmed.to_lowercase();
    PRESCREEN_ENGINEERING_VERBS
        .iter()
        .any(|kw| lowered.contains(kw))
}

/// 从 LLM 回复文本中容错提取分诊档位。
///
/// 容错规则(全部大小写不敏感,在文本任意位置做子串匹配):
/// - 含 `trivial` 且不含 `standard` → [`TriageTier::Trivial`];
/// - 其余(含 `standard`,或两者皆无,或两者同时出现)→
///   [`TriageTier::Standard`] 保守兜底 —— 宁可不裁剪,也不误伤工程轮次。
pub fn parse_triage_label(text: &str) -> TriageTier {
    let lowered = text.to_lowercase();
    if lowered.contains("trivial") && !lowered.contains("standard") {
        TriageTier::Trivial
    } else {
        TriageTier::Standard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在锁内设置(或清除)`CLAW_TRIAGE_MODE`,执行断言后恢复。
    ///
    /// 与 conversation.rs 集成测试共享 [`TRIAGE_ENV_LOCK`],避免并行污染。
    fn with_triage_mode(value: Option<&str>, assert_fn: impl FnOnce()) {
        let guard = TRIAGE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match value {
            Some(v) => std::env::set_var(TRIAGE_MODE_ENV, v),
            None => std::env::remove_var(TRIAGE_MODE_ENV),
        }
        assert_fn();
        std::env::remove_var(TRIAGE_MODE_ENV);
        drop(guard);
    }

    // ── parse_triage_label:容错解析 ──

    /// 精确单词输出 → Trivial。
    #[test]
    fn parse_exact_trivial() {
        assert_eq!(parse_triage_label("trivial"), TriageTier::Trivial);
    }

    /// 大小写不敏感 + 首尾空白 → Trivial。
    #[test]
    fn parse_trivial_case_insensitive_and_trimmed() {
        assert_eq!(parse_triage_label("Trivial "), TriageTier::Trivial);
        assert_eq!(parse_triage_label("  TRIVIAL\n"), TriageTier::Trivial);
    }

    /// 精确单词输出(带空白)→ Standard。
    #[test]
    fn parse_exact_standard_with_whitespace() {
        assert_eq!(parse_triage_label("  standard\n"), TriageTier::Standard);
        assert_eq!(parse_triage_label("Standard"), TriageTier::Standard);
    }

    /// 带前后缀文本(模型夹带解释时仍能容错命中)。
    #[test]
    fn parse_label_embedded_in_surrounding_text() {
        assert_eq!(
            parse_triage_label("结论:trivial(仅状态问答)"),
            TriageTier::Trivial
        );
        assert_eq!(
            parse_triage_label("This is standard — 属于工程任务"),
            TriageTier::Standard
        );
    }

    /// 无任何匹配(含空文本)→ Standard 保守兜底。
    #[test]
    fn parse_no_match_falls_back_to_standard() {
        assert_eq!(parse_triage_label(""), TriageTier::Standard);
        assert_eq!(parse_triage_label("   \n\t  "), TriageTier::Standard);
        assert_eq!(parse_triage_label("我不知道怎么分类"), TriageTier::Standard);
        assert_eq!(parse_triage_label("OK done"), TriageTier::Standard);
    }

    /// 两词同时出现(模型输出混乱)→ Standard(保守优先,不误伤工程轮次)。
    #[test]
    fn parse_ambiguous_both_words_falls_back_to_standard() {
        assert_eq!(
            parse_triage_label("not trivial, is standard"),
            TriageTier::Standard
        );
        assert_eq!(parse_triage_label("trivial standard"), TriageTier::Standard);
    }

    // ── triage_llm_enabled:env 解析 ──

    /// 缺失 → 默认 llm(开启入口 LLM 分诊)。
    #[test]
    fn enabled_defaults_to_llm_when_env_missing() {
        with_triage_mode(None, || {
            assert!(triage_llm_enabled(), "缺失 env 应默认开启 LLM 分诊");
        });
    }

    /// 显式 `llm` → 开启。
    #[test]
    fn enabled_when_env_is_llm() {
        with_triage_mode(Some("llm"), || {
            assert!(triage_llm_enabled());
        });
    }

    /// 显式 `off`(大小写不敏感)→ 关闭 LLM 分诊(调用方跳过分诊)。
    #[test]
    fn disabled_when_env_is_off() {
        with_triage_mode(Some("off"), || {
            assert!(!triage_llm_enabled(), "off 应关闭 LLM 分诊");
        });
        with_triage_mode(Some("OFF"), || {
            assert!(!triage_llm_enabled(), "大小写不敏感");
        });
    }

    /// `rule`(旧纯规则语义)不再是特例 → 按默认 `llm` 处理(开启分诊)。
    #[test]
    fn enabled_when_env_is_legacy_rule() {
        with_triage_mode(Some("rule"), || {
            assert!(
                triage_llm_enabled(),
                "rule 语义已移除,应回退默认 llm(开启分诊)"
            );
        });
        with_triage_mode(Some("RULE"), || {
            assert!(triage_llm_enabled(), "大小写不敏感");
        });
    }

    /// 非法值 → 按默认 `llm` 处理(保守选择,开启 LLM 分诊)。
    #[test]
    fn enabled_when_env_is_invalid() {
        with_triage_mode(Some("bogus"), || {
            assert!(triage_llm_enabled(), "非法值应回退默认 llm");
        });
    }

    // ── prescreen_standard:本地零成本预筛(2026-10-09)──

    /// 长输入(≥ 阈值) → Standard,免一次 LLM 往返。
    #[test]
    fn prescreen_long_input_is_standard() {
        let long = "帮我看看这段代码为什么会偶发失败并且给出根因".repeat(2);
        assert!(prescreen_standard(&long), "长输入应预筛为 Standard");
    }

    /// 含路径分隔符 → Standard。
    #[test]
    fn prescreen_path_reference_is_standard() {
        assert!(prescreen_standard("看下 src/main.rs"));
        assert!(prescreen_standard("打开 D:\\proj\\a.py"));
    }

    /// 明确工程动作词 → Standard(中英文,ASCII 大小写不敏感)。
    #[test]
    fn prescreen_engineering_verb_is_standard() {
        assert!(prescreen_standard("修复它"));
        assert!(prescreen_standard("refactor this"));
        assert!(prescreen_standard("FIX it"));
    }

    /// 关键安全性质:状态问答**不得**被预筛为 Standard。
    #[test]
    fn prescreen_status_questions_are_not_standard() {
        assert!(!prescreen_standard("上次任务执行到哪里了"));
        assert!(!prescreen_standard("当前工作目录是什么"));
        assert!(!prescreen_standard("这个报错信息是什么意思"));
    }

    /// 空/空白输入 → 不判定(false),交由既有 LLM 分诊。
    #[test]
    fn prescreen_empty_is_not_determined() {
        assert!(!prescreen_standard(""));
        assert!(!prescreen_standard("   \n\t "));
    }

    /// 「执行/检查/分析」等歧义词**不得**单独触发预筛(否则会削弱精度)。
    #[test]
    fn prescreen_ambiguous_verbs_do_not_trigger() {
        assert!(!prescreen_standard("执行"));
        assert!(!prescreen_standard("检查"));
        assert!(!prescreen_standard("分析"));
    }
}
