//! 入口全量 LLM 分诊(轮次分诊门升级)—— 把纯规则 triage 升级为
//! 「每个用户 turn 在请求构造前,用一次轻量 LLM 调用判断档位」的路由器模式。
//!
//! # 背景
//! 纯规则分诊([`crate::triage::triage_turn`])是词汇近似,命中率低(真实回放
//! 102 条仅 1 条 Trivial),无法语义识别任务类型,且词表需要不断打补丁。
//! 升级后:
//! - 入口用一次轻量 LLM 调用(deepseek-v4-flash)判断本轮档位;
//! - [`crate::triage::triage_turn`] 纯规则降级为 fallback(LLM 调用失败/
//!   被禁用/`with_model` 不可用时使用),**语义不变**;
//! - 此 router 模式未来可复用到其它"纯规则判断不达标"的场景。
//!
//! 本模块只含纯函数(system prompt / 解析 / 开关),不依赖 `ApiClient` 类型,
//! 全部可离线单测;异步调用接入见
//! [`crate::conversation::ConversationRuntime::triage_turn_llm`]。
//!
//! # 「关闭思考」的实际形态(以代码为准,2026-09-04 勘察)
//! - `deepseek-v4-flash` 就是普通模型别名(`resolve_model_alias("flash")` →
//!   `"deepseek-v4-flash"`,见 api::providers),**不存在独立的"无思考"模型名**。
//! - API 层思考开关是 `api::MessageRequest.thinking_mode: Option<bool>`:
//!   `Some(false)` → 请求体注入 `"thinking": {"type": "disabled"}`
//!   (仅 DeepSeek 模型,见 api::providers::openai_compat 的 payload 构造);
//!   `None` → 不发送该字段,走后端默认(DeepSeek V4 默认开思考)。
//! - runtime 的 [`ApiRequest`](crate::conversation::ApiRequest)(trait 边界)
//!   **没有** thinking 字段;生产 `AnthropicRuntimeClient`(rusty-claude-cli)
//!   把 `thinking_mode` 作为 client 级状态(构造默认 `None`,由 CLI
//!   `--no-thinking` 设置),`with_model(...)` 新建的 client 同样为 `None`。
//! - 因此本实现直接使用普通 `deepseek-v4-flash` 别名发请求,不显式携带
//!   thinking 参数(该参数位在 runtime → API 的 trait 边界不可达);若要在
//!   请求体显式 `thinking: disabled`,需在 CLI/client 层让 `with_model`
//!   继承主 client 的 `thinking_mode`(超出本模块范围)。分诊解析只取
//!   assistant 文本块 —— 即便模型默认思考,thinking 块也被忽略,输出不受影响。

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
/// 宁可不裁剪也不误伤正常工程轮次。全静态文本(单 section),
/// 同内容请求可命中 DeepSeek prefix cache。
pub const TRIAGE_LLM_SYSTEM: &str = "\
你是任务分诊器。根据用户输入判断本轮对话应注入哪种上下文档位,只输出一个词:trivial 或 standard。

- trivial:纯状态/问答/简单说明类轮次。典型例子:「上次任务执行到哪里了」
  「当前工作目录是什么」「任务完成的怎么样了」「这个报错信息是什么意思」。
  这类轮次只需状态信息与核心工具即可回答,不需要完整工具目录与
  多 Agent / 架构教程。
- standard:任何工程任务或不确定的情况。典型例子:修改/修复/实现/重构/
  调试/检查分析代码、前端调整、写脚本、执行多步任务等。

保守优先:不确定一律 standard —— 宁可不裁剪,也不误伤正常工程轮次。

只输出一个词(trivial 或 standard),禁止输出任何其他内容、解释、标点或引号。";

/// `CLAW_TRIAGE_MODE` 环境变量名。
///
/// 取值:
/// - `llm`(默认):入口全量 LLM 分诊;
/// - `rule`:纯规则降级([`crate::triage::triage_turn`])。
pub const TRIAGE_MODE_ENV: &str = "CLAW_TRIAGE_MODE";

/// 串行化「触碰 `CLAW_TRIAGE_MODE` env」的测试。
///
/// 与既有 `COST_ENV_LOCK`(cost_meter.rs)同模式:设置 env 的测试与依赖默认
/// 分诊模式的集成测试并行运行会互相污染,须共享同一把锁。`#[cfg(test)]` 下
/// 定义,conversation.rs 集成测试经 `crate::triage_llm::TRIAGE_ENV_LOCK`
/// 引用(仅测试构建存在)。
#[cfg(test)]
pub(crate) static TRIAGE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 入口全量 LLM 分诊是否启用(env `CLAW_TRIAGE_MODE` 解析)。
///
/// - 值 `rule`(大小写不敏感)→ `false`:回退纯规则分诊;
/// - 值 `llm` / 缺失 / 任何非法值 → `true`:开启入口 LLM 分诊(默认)。
pub fn triage_llm_enabled() -> bool {
    !std::env::var(TRIAGE_MODE_ENV).is_ok_and(|v| v.eq_ignore_ascii_case("rule"))
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

    /// 显式 `rule`(大小写不敏感)→ 关闭,走纯规则。
    #[test]
    fn disabled_when_env_is_rule() {
        with_triage_mode(Some("rule"), || {
            assert!(!triage_llm_enabled(), "rule 应关闭 LLM 分诊");
        });
        with_triage_mode(Some("RULE"), || {
            assert!(!triage_llm_enabled(), "大小写不敏感");
        });
    }

    /// 非法值 → 按默认 `llm` 处理(保守选择,开启 LLM 分诊)。
    #[test]
    fn enabled_when_env_is_invalid() {
        with_triage_mode(Some("bogus"), || {
            assert!(triage_llm_enabled(), "非法值应回退默认 llm");
        });
    }
}
