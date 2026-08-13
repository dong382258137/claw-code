use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const COMPACT_CONTINUATION_PREAMBLE: &str =
    "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n";
const COMPACT_RECENT_MESSAGES_NOTE: &str = "Recent messages are preserved verbatim.";
// 中性续接指令 v2:压缩 summary 描述的是"压缩时刻之前"的工作,可能含多个
// 不同状态的任务。原指令 "Continue the conversation from where it left off"
// 会把 agent 拉回压缩前的旧任务 —— 当用户随后切换新任务或旧任务已收尾时,
// 该残留指令成为任务漂移源(实测:用户说"继续",AI 却去继续压缩前已完成
// 的任务)。v2 增加 active/closed 任务区分引导:已收尾任务(PASS/已修复/已
// 完成/已交付)即使摘要中描述再详细,也禁止续接。
const COMPACT_DIRECT_RESUME_INSTRUCTION: &str = "Continue the conversation. \
The summary above describes earlier work and may describe tasks in different states; \
treat it as a structured record and distinguish ACTIVE tasks from CLOSED tasks. \
CLOSED tasks are finished — marked by indicators like PASS, 已修复/已收尾/已完成, \
delivered, or explicitly summarized as done — and MUST NOT be resumed, even if the \
most recent user message is \"continue\". If the most recent user message starts a new \
task or is unrelated, focus on it instead of any summarized work; if the summary is \
stale relative to the current request, ignore it. Resume directly — do not acknowledge \
the summary, do not recap what was happening, and do not preface with continuation text.";

const COMPACT_BOUNDARY_MARKER_PREFIX: &str = "<!-- compact_boundary: ";
const COMPACT_BOUNDARY_MARKER_SUFFIX: &str = " -->";

/// LLM 摘要 client — 由上层 CLI 注入,使 context compaction 生成
/// **模型摘要**而非纯启发式规则摘要(context editing 的核心能力)。
///
/// 同步 trait(与 `decision_log::DecisionExtractorClient` 同构),生产实现
/// (`rusty-claude-cli::llm_clients::DeepSeekCompactionSummarizerClient`)
/// 内部用独立 tokio runtime + ProviderClient 桥接异步 API。
///
/// 未注册或调用失败时,[`summarize_messages_with_llm`] 自动降级为启发式,
/// 保证压缩永不因 LLM 故障阻塞。
pub trait CompactionSummarizerClient: Send + Sync {
    /// 输入压缩 prompt,返回模型生成的摘要文本(自由格式文本,建议项目符号)。
    fn summarize(&self, prompt: &str) -> Result<String, String>;
}

/// 全局压缩摘要 client(OnceLock,进程级单例)。
///
/// 设计镜像 `decision_log::GLOBAL_DECISION_EXTRACTOR`:
/// - 通过 [`set_global_compaction_summarizer_client`] 在进程启动时注入
/// - 未注册时 `summarize_messages_with_llm` 走纯启发式(零 LLM 成本)
static GLOBAL_COMPACTION_SUMMARIZER: OnceLock<Option<Arc<dyn CompactionSummarizerClient>>> =
    OnceLock::new();

/// 注册全局压缩摘要 client(进程级单例,重复注册静默忽略)。
pub fn set_global_compaction_summarizer_client(client: Arc<dyn CompactionSummarizerClient>) {
    let _ = GLOBAL_COMPACTION_SUMMARIZER.set(Some(client));
}

/// 是否已注册压缩摘要 client(供诊断/命令展示使用)。
#[must_use]
pub fn is_compaction_summarizer_registered() -> bool {
    GLOBAL_COMPACTION_SUMMARIZER
        .get()
        .is_some_and(|slot| slot.is_some())
}

fn global_compaction_summarizer() -> Option<Arc<dyn CompactionSummarizerClient>> {
    GLOBAL_COMPACTION_SUMMARIZER
        .get()
        .and_then(|slot| slot.clone())
}

/// 构建 LLM 压缩 prompt — 把待压缩消息渲染成精简转录 + 摘要指令。
///
/// 转录复用 [`summarize_block`] 的逐块摘要(每条消息一行),控制 prompt 体积;
/// 指令要求输出项目符号形式的要点,保证 [`merge_compact_summaries`]
/// 能通过 [`extract_summary_highlights`] 提取到内容。
fn build_llm_summarize_prompt(messages: &[ConversationMessage]) -> String {
    let mut transcript = Vec::new();
    // P0(2026-08-14):跳过 System 角色消息 —— 压缩窗口内含上一轮压缩的
    // `<summary>` system 消息,LLM 会把它当正文回声复述(逐条拷贝旧摘要的
    // Scope:/Tools mentioned:/Key timeline: 等行),造成新摘要重复混乱
    // (实测 1786557173235 会话:LLM 摘要末尾整段回声旧启发式摘要)。
    // 旧摘要由 merge_compact_summaries 单独合并(previous highlights cap 9 行)。
    for (idx, message) in messages
        .iter()
        .filter(|message| message.role != MessageRole::System)
        .enumerate()
    {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let content = message
            .blocks
            .iter()
            .map(summarize_block)
            .collect::<Vec<_>>()
            .join(" | ");
        transcript.push(format!("{idx}. {role}: {content}"));
    }
    format!(
        "You are a session-compaction summarizer. The following lines are a condensed transcript \
         of an earlier portion of a coding-assistant conversation (roles: user / assistant / tool).\n\
         \n\
         {}\n\
         \n\
         Write a concise but information-dense summary that lets the assistant continue the task \
         without the original transcript. Requirements:\n\
         - Cover: the user's goals and recent requests, what was implemented/decided, key file \
         paths touched, tool calls that matter, and any pending work.\n\
         - Output plain bullet lines, one per point, each starting with \"- \".\n\
         - Do NOT wrap in <summary> tags, do NOT add a \"- Key timeline:\" header, do not include \
         per-message trivia. Keep it under 60 lines.\n\
         - Preserve Chinese/English content verbatim where it matters (identifiers, file paths).\n\
         \n\
         After the bullets, ALWAYS append three machine-parseable sections (P1: task-state fieldization + \
         failure-lessons fieldization, mirrors Claude Code's structured compaction):\n\
         \n\
         [active_task]\n\
         goal: <the single current task objective; if no task is in progress, write NONE>\n\
         next_action: <the immediate next step; if the task is finished, write NONE>\n\
         \n\
         [closed_tasks]\n\
         - <finished tasks, one bullet each, with a completion marker such as PASS/FAIL/已修复/已完成; \
         if none, write NONE>\n\
         \n\
         [lessons]\n\
         - <operational lessons worth persisting, one bullet each: failed or inefficient tool operations \
         (e.g. wrong path, wrong git invocation, permission errors), their cause and how to avoid them; \
         this includes mistakes that were later recovered even if the overall turn succeeded. \
         Each lesson must be self-contained and actionable for a future session; if none, write NONE>\n\
         \n\
         The sections above are consumed by a parser — keep the headers exactly as shown, one field per \
         line, and put them AFTER the summary bullets.",
        transcript.join("\n")
    )
}

/// 把模型自由格式输出规范化为 `<summary>` 块,保证下游
/// [`merge_compact_summaries`] / [`compress_summary_text`] 能正确解析。
fn sanitize_llm_summary(text: &str) -> String {
    let body = text
        .trim()
        .trim_start_matches("<summary>")
        .trim_end_matches("</summary>")
        .trim();
    let mut lines = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 确保每行是 bullet,否则 extract_summary_highlights 提取不到。
        if line.starts_with("- ") {
            lines.push(line.to_string());
        } else {
            lines.push(format!("- {line}"));
        }
    }
    if lines.is_empty() {
        lines.push("- (LLM summary returned empty content)".to_string());
    }
    format!(
        "<summary>\nConversation summary:\n{}\n</summary>",
        lines.join("\n")
    )
}

/// 生成待压缩消息的摘要 — **LLM 优先,启发式兜底**。
///
/// 从全局注册表取 client;未注册 / 调用失败 / 返回空 → 回退
/// [`summarize_messages`] 启发式规则摘要。
fn summarize_messages_with_llm(messages: &[ConversationMessage]) -> String {
    // Tier S #3 穷鬼模式:激活时跳过 LLM 摘要,直接回退启发式(省 token)。
    if crate::poor_mode::is_active() {
        return summarize_messages(messages);
    }
    summarize_messages_with_client(messages, global_compaction_summarizer().as_deref())
}

/// 带显式 client 的摘要生成(测试可注入 fake client,生产走全局注册表)。
fn summarize_messages_with_client(
    messages: &[ConversationMessage],
    client: Option<&dyn CompactionSummarizerClient>,
) -> String {
    if let Some(client) = client {
        let prompt = build_llm_summarize_prompt(messages);
        match client.summarize(&prompt) {
            Ok(text) if !text.trim().is_empty() => return sanitize_llm_summary(&text),
            Ok(_) => {
                eprintln!(
                    "[compact] LLM summarizer returned empty output; falling back to heuristic"
                );
            }
            Err(e) => {
                eprintln!("[compact] LLM summarizer failed ({e}); falling back to heuristic");
            }
        }
    } else {
        // P2(2026-08-14):未注册时静默降级是"摘要质量极差"的隐性来源
        // (启发式会把原始转录碎片拼进 Key timeline)。打印一次可见告警,
        // 便于用户判断是注册失败还是瞬时 LLM 故障。
        eprintln!(
            "[compact] LLM summarizer not registered; falling back to heuristic summary"
        );
    }
    summarize_messages(messages)
}

/// 为整段会话生成摘要(不压缩,不删除消息)— 供 `/summary` 命令等展示用途。
///
/// 同样走 LLM 优先、启发式兜底,输出为 `<summary>` 块文本。
#[must_use]
pub fn render_session_summary(session: &Session) -> String {
    summarize_messages_with_llm(&session.messages)
}

/// Identifies what triggered a compaction pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactTrigger {
    Auto,
    Manual,
    Reactive,
}

/// Metadata embedded in the post-compaction System message so downstream
/// request construction can slice the transcript at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactBoundary {
    pub trigger: CompactTrigger,
    pub pre_tokens: usize,
    pub messages_summarized: usize,
    pub timestamp_ms: u64,
}

/// Formats a [`CompactBoundary`] as a machine-parseable HTML comment marker line.
#[must_use]
fn format_compact_boundary_marker(boundary: &CompactBoundary) -> String {
    let json = serde_json::to_string(boundary).unwrap_or_else(|_| "{}".to_string());
    format!("{COMPACT_BOUNDARY_MARKER_PREFIX}{json}{COMPACT_BOUNDARY_MARKER_SUFFIX}")
}

/// Parses the most recent [`CompactBoundary`] marker from a text block, if any.
#[allow(dead_code)]
fn parse_compact_boundary_from_text(text: &str) -> Option<CompactBoundary> {
    let marker_start = text.rfind(COMPACT_BOUNDARY_MARKER_PREFIX)?;
    let after_prefix = &text[marker_start + COMPACT_BOUNDARY_MARKER_PREFIX.len()..];
    let end = after_prefix.find(COMPACT_BOUNDARY_MARKER_SUFFIX)?;
    let json_str = &after_prefix[..end];
    serde_json::from_str(json_str).ok()
}

/// Returns true when the text contains a compact_boundary marker.
fn has_compact_boundary_marker(text: &str) -> bool {
    text.contains(COMPACT_BOUNDARY_MARKER_PREFIX)
}

/// Strips a trailing compact_boundary marker line from `text`, if present.
fn strip_compact_boundary_marker(text: &str) -> &str {
    if let Some(idx) = text.rfind(COMPACT_BOUNDARY_MARKER_PREFIX) {
        let trimmed = text[..idx].trim_end_matches(['\n', '\r', ' ']);
        trimmed
    } else {
        text
    }
}

/// Thresholds controlling when and how a session is compacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    pub preserve_recent_messages: usize,
    pub max_estimated_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            preserve_recent_messages: 4,
            max_estimated_tokens: 10_000,
        }
    }
}

/// Result of compacting a session into a summary plus preserved tail messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub summary: String,
    pub formatted_summary: String,
    pub compacted_session: Session,
    pub removed_message_count: usize,
}

/// Roughly estimates the token footprint of the current session transcript.
#[must_use]
pub fn estimate_session_tokens(session: &Session) -> usize {
    session.messages.iter().map(estimate_message_tokens).sum()
}

/// Returns `true` when the session exceeds the configured compaction budget.
#[must_use]
pub fn should_compact(session: &Session, config: CompactionConfig) -> bool {
    let start = compacted_summary_prefix_len(session);
    let compactable = &session.messages[start..];

    compactable.len() > config.preserve_recent_messages
        && compactable
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>()
            >= config.max_estimated_tokens
}

/// Normalizes a compaction summary into user-facing continuation text.
#[must_use]
pub fn format_compact_summary(summary: &str) -> String {
    let without_analysis = strip_tag_block(summary, "analysis");
    let formatted = if let Some(content) = extract_tag_block(&without_analysis, "summary") {
        without_analysis.replace(
            &format!("<summary>{content}</summary>"),
            &format!("Summary:\n{}", content.trim()),
        )
    } else {
        without_analysis
    };

    collapse_blank_lines(&formatted).trim().to_string()
}

/// Builds the synthetic system message used after session compaction.
#[must_use]
pub fn get_compact_continuation_message(
    summary: &str,
    suppress_follow_up_questions: bool,
    recent_messages_preserved: bool,
) -> String {
    let mut base = format!(
        "{COMPACT_CONTINUATION_PREAMBLE}{}",
        format_compact_summary(summary)
    );

    if recent_messages_preserved {
        base.push_str("\n\n");
        base.push_str(COMPACT_RECENT_MESSAGES_NOTE);
    }

    if suppress_follow_up_questions {
        base.push('\n');
        base.push_str(COMPACT_DIRECT_RESUME_INSTRUCTION);
    }

    base
}

/// Compacts a session by summarizing older messages and preserving the recent tail.
///
/// This is the default entry point and records the compaction with
/// [`CompactTrigger::Auto`]. Use [`compact_session_with_trigger`] to embed a
/// different trigger in the boundary marker.
#[must_use]
pub fn compact_session(session: &Session, config: CompactionConfig) -> CompactionResult {
    compact_session_with_trigger(session, config, CompactTrigger::Auto)
}

/// Compacts a session and embeds a [`CompactBoundary`] marker in the
/// post-compaction System message so request construction can slice the
/// transcript at the boundary (see [`get_messages_after_compact_boundary`]).
#[must_use]
pub fn compact_session_with_trigger(
    session: &Session,
    config: CompactionConfig,
    trigger: CompactTrigger,
) -> CompactionResult {
    if !should_compact(session, config) {
        return CompactionResult {
            summary: String::new(),
            formatted_summary: String::new(),
            compacted_session: session.clone(),
            removed_message_count: 0,
        };
    }

    let pre_tokens = estimate_session_tokens(session);
    let existing_summary = session
        .messages
        .first()
        .and_then(extract_existing_compacted_summary);
    let compacted_prefix_len = usize::from(existing_summary.is_some());
    let raw_keep_from = session
        .messages
        .len()
        .saturating_sub(config.preserve_recent_messages);
    // Ensure we do not split a tool-use / tool-result pair at the compaction
    // boundary. If the first preserved message is a user message whose first
    // block is a ToolResult, the assistant message with the matching ToolUse
    // was slated for removal — that produces an orphaned tool role message on
    // the OpenAI-compat path (400: tool message must follow assistant with
    // tool_calls). Walk the boundary back until we start at a safe point.
    let keep_from = {
        let mut k = raw_keep_from;
        // If the first preserved message is a tool-result turn, ensure its
        // paired assistant tool-use turn is preserved too. Without this fix,
        // the OpenAI-compat adapter sends an orphaned 'tool' role message
        // with no preceding assistant 'tool_calls', which providers reject
        // with a 400. We walk back only if the immediately preceding message
        // is NOT an assistant message that contains a ToolUse block (i.e. the
        // pair is actually broken at the boundary).
        loop {
            if k == 0 || k <= compacted_prefix_len {
                break;
            }
            let first_preserved = &session.messages[k];
            let starts_with_tool_result = first_preserved
                .blocks
                .first()
                .is_some_and(|b| matches!(b, ContentBlock::ToolResult { .. }));
            if !starts_with_tool_result {
                break;
            }
            // Check the message just before the current boundary.
            let preceding = &session.messages[k - 1];
            let preceding_has_tool_use = preceding
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
            if preceding_has_tool_use {
                // Pair is intact — walk back one more to include the assistant turn.
                k = k.saturating_sub(1);
                break;
            }
            // Preceding message has no ToolUse but we have a ToolResult —
            // this is already an orphaned pair; walk back to try to fix it.
            k = k.saturating_sub(1);
        }
        k
    };
    let removed = &session.messages[compacted_prefix_len..keep_from];
    let preserved = session.messages[keep_from..].to_vec();
    // LLM 优先摘要(context editing):已注册 CompactionSummarizerClient 时
    // 走模型摘要(语义压缩),未注册/失败回退启发式 summarize_messages。
    let new_summary = summarize_messages_with_llm(removed);
    let merged_summary = merge_compact_summaries(existing_summary.as_deref(), &new_summary);
    // Compress the merged summary to bound its size (max 1200 chars / 24 lines by
    // default). Without this, repeated compactions accumulate highlights and the
    // summary grows unbounded, wasting tokens every subsequent turn.
    let summary = crate::summary_compression::compress_summary_text(&merged_summary);
    let formatted_summary = format_compact_summary(&summary);
    let continuation = get_compact_continuation_message(&summary, true, !preserved.is_empty());

    let boundary = CompactBoundary {
        trigger,
        pre_tokens,
        messages_summarized: removed.len(),
        timestamp_ms: current_time_millis(),
    };
    let continuation_with_marker = format!(
        "{continuation}\n{}",
        format_compact_boundary_marker(&boundary)
    );

    let mut compacted_messages = vec![ConversationMessage {
        role: MessageRole::System,
        blocks: vec![ContentBlock::Text {
            text: continuation_with_marker,
        }],
        usage: None,
    }];
    compacted_messages.extend(preserved);

    let mut compacted_session = session.clone();
    compacted_session.messages = compacted_messages;
    compacted_session.record_compaction(summary.clone(), removed.len());

    CompactionResult {
        summary,
        formatted_summary,
        compacted_session,
        removed_message_count: removed.len(),
    }
}

/// Returns the slice of `messages` starting from the most recent System message
/// that contains a compact_boundary marker (inclusive) to the end. If no
/// boundary marker is present, the entire slice is returned unchanged.
///
/// This lets request construction drop stale pre-compaction messages that may
/// have been left in the transcript by partial writes or session restoration.
#[must_use]
pub fn get_messages_after_compact_boundary(
    messages: &[ConversationMessage],
) -> &[ConversationMessage] {
    let boundary_idx = messages.iter().rposition(|message| {
        message.role == MessageRole::System
            && message.blocks.iter().any(|block| match block {
                ContentBlock::Text { text } => has_compact_boundary_marker(text),
                _ => false,
            })
    });
    match boundary_idx {
        Some(idx) => &messages[idx..],
        None => messages,
    }
}

/// Extracts the most recent [`CompactBoundary`] from a message slice, if any.
#[must_use]
#[allow(dead_code)]
pub fn extract_compact_boundary(messages: &[ConversationMessage]) -> Option<CompactBoundary> {
    messages
        .iter()
        .rev()
        .filter(|message| message.role == MessageRole::System)
        .find_map(|message| {
            message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => parse_compact_boundary_from_text(text),
                _ => None,
            })
        })
}

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

// ---- Microcompact: tool result summarization ----

/// Tool names whose outputs are safe to summarize once they age out of the
/// recent window. These tools produce large read-only payloads (file contents,
/// command output, search hits) that are not needed verbatim in later turns.
const SUMMARIZABLE_TOOLS: &[&str] = &["Read", "Bash", "Grep", "Glob", "LS"];

/// Tool names whose results must never be summarized because the verbatim
/// output is required for the model to reason about subsequent state changes.
///
/// P1 改进:新增 `dispatch_subagent` / `check_subagent` — 子智能体结果包含
/// 关键指针(subagent_id + result_ref 路径),丢失会导致主 agent 无法引用
/// 子智能体的工作产物。这些结果体积小(通常 < 200 字符),保留完整不会
/// 显著增加上下文负担。与 NOTEBOOK 协同:NOTEBOOK.md 的 `<subagents>` 段
/// 提供跨压缩持久化,但单 turn 内的指针仍需完整保留以避免"game of telephone"。
const CRITICAL_TOOLS: &[&str] = &[
    "Edit",
    "Write",
    "Delete",
    "dispatch_subagent",
    "check_subagent",
];

/// Returns true when `tool_name` produces high-volume read-only output that is
/// safe to summarize.
fn is_summarizable_tool(tool_name: &str) -> bool {
    SUMMARIZABLE_TOOLS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(tool_name))
}

/// Returns true when `tool_name` performs state mutations whose results must be
/// preserved verbatim.
fn is_critical_tool(tool_name: &str) -> bool {
    CRITICAL_TOOLS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(tool_name))
}

/// Returns true when `output` already looks like a microcompact summary, so we
/// avoid re-summarizing an already-summarized result.
///
/// Phase 2 内容感知路由:兼容新旧两种格式。
/// - 旧格式:`[{tool_name} output summarized: ... chars → ...…]`
/// - 新格式:`[{tool_name} {Text|JSON|Code|Tabular} summarized: ... chars → ...…]`
///
/// 统一检测子串 `" summarized: "`(去掉前缀的 tool_name 部分),让新旧格式
/// 都能被识别。`starts_with('[')` + `ends_with("…]")` + `contains(" chars → ")`
/// 三个条件联合保证不会误判正常 tool result(以 `[` 开头且以 `…]` 结尾且
/// 含特殊字符的 tool result 极罕见)。
fn is_already_summarized(output: &str) -> bool {
    output.starts_with('[')
        && output.contains(" summarized: ")
        && output.ends_with("…]")
        && output.contains(" chars → ")
}

/// Builds the summary placeholder for an aged tool result.
///
/// Phase 2 内容感知路由:此函数现在是路由入口,委托给
/// `content_compression::format_summary`,根据内容类型(JSON/Code/Tabular/Text)
/// 选择对应的压缩器。详见 `docs/design-headroom-absorption.md` 第 2 节。
///
/// 各压缩器输出统一格式的 placeholder(以 `[` 开头,`…]` 结尾,含
/// `" summarized: "` 和 `" chars → "`),确保 `is_already_summarized` 兼容。
///
/// - JSON 压缩器:保留完整 JSON 结构,压缩叶子值(string 截断、数组省略)
/// - Code 压缩器:保留签名(fn/impl/struct/use),折叠实现体
/// - Tabular 压缩器:保留表头 + 前 3 行代表性行
/// - Text 压缩器:原"前 3 行 + 240 chars"逻辑(兜底)
#[must_use]
fn format_tool_result_summary(
    tool_name: &str,
    tool_use_id: &str,
    input: &str,
    output: &str,
) -> String {
    crate::content_compression::format_summary(tool_name, tool_use_id, input, output)
}

/// 输出字符数低于此阈值的 tool result 不值得摘要:体积小、不占上下文,
/// 摘要反而丢失精确内容,导致模型重复查询。直接保留原文。
const SMALL_OUTPUT_PRESERVE_CHARS: usize = 200;

/// Summarize old tool results to free context before full compaction.
///
/// - `Read`/`Bash`/`Grep`/`Glob`/`LS` results older than `preserve_recent`
///   turns are replaced with a one-line summary placeholder.
/// - `Edit`/`Write`/`Delete` results are kept verbatim so the model can still
///   reason about state changes.
/// - Tool results with `is_error = true` are always kept verbatim.
/// - The most recent `preserve_recent` tool results (of any kind) are kept
///   verbatim so the active working set remains visible.
///
/// 这是 `microcompact_with_archiver` 的便捷包装,不进行归档。
/// 需要归档原始 tool result 时,请使用 [`microcompact_with_archiver`]。
#[must_use]
pub fn microcompact(
    messages: &[ConversationMessage],
    preserve_recent: usize,
) -> Vec<ConversationMessage> {
    microcompact_with_archiver(messages, preserve_recent, |_, _, _| {})
}

/// 与 [`microcompact`] 行为相同,但在替换 `ToolResult.output` 为摘要前,
/// 调用 `archiver(tool_use_id, tool_name, original_output)` 让调用方归档原始内容。
///
/// # P0:真无损压缩
///
/// 旧版 microcompact 直接用 `format_tool_result_summary` 覆盖 `output`,
/// 原始内容永久丢失。本函数在覆盖前调用 `archiver`,让调用方把原始内容
/// 持久化到 `ToolResultArchive`(`.claw/tool_results_archive.jsonl`)。
/// LLM 后续可通过 `recall_full` 工具按 `tool_use_id` 主动检索原始内容。
///
/// # archiver 调用时机
///
/// - 仅在被摘要的工具(`Read`/`Bash`/`Grep`/`Glob`/`LS`)上调用
/// - 不会在 critical tool(Edit/Write/Delete)或 error 上调用
/// - 不会在已摘要的 output 上重复调用(避免双重归档)
///
/// # archiver 错误处理
///
/// archiver 是 `FnMut(&str, &str, &str) -> ()`,不返回 Result。
/// 调用方应在 archiver 内部吞掉错误(归档失败不阻断 microcompact):
///
/// ```ignore
/// microcompact_with_archiver(messages, preserve_recent, |id, name, output| {
///     let _ = archive_tool_result(&workspace_root, id, name, output);
/// })
/// ```
#[must_use]
pub fn microcompact_with_archiver<F>(
    messages: &[ConversationMessage],
    preserve_recent: usize,
    mut archiver: F,
) -> Vec<ConversationMessage>
where
    F: FnMut(&str, &str, &str),
{
    // Collect the indices of messages that contain at least one ToolResult
    // block. Each such message is treated as one "tool result unit" for the
    // recency window.
    let tool_result_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .map(|(idx, _)| idx)
        .collect();

    // The most recent `preserve_recent` tool-result messages are kept intact.
    // Older ones become candidates for summarization.
    let preserve_count = preserve_recent.min(tool_result_indices.len());
    let cutoff = tool_result_indices.len().saturating_sub(preserve_count);
    let mut summarize_candidates: HashSet<usize> =
        tool_result_indices[..cutoff].iter().copied().collect();

    // 依赖感知保护(Self-GC arXiv:2607.00692):按时间序产生的候选摘要集
    // 对"未来依赖"盲目 —— 被后续 assistant 文本引用过的工具结果(如 AI 的
    // "引擎复现是 6 笔"引用之前的 bash 输出)一旦被压成摘要,模型只能重新
    // 调用工具查询,造成重复劳动与 token 浪费。故从候选集中剔除两类:
    // 1) 被后续 assistant 文本提及工具名的结果(正在被讨论/引用的活跃证据)
    // 2) 输出本身很小(< SMALL_OUTPUT_PRESERVE_CHARS)的结果(摘要无收益)
    let mut protected: HashSet<usize> = HashSet::new();
    for &idx in &tool_result_indices[..cutoff] {
        let message = &messages[idx];
        let tool_names: Vec<String> = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_name, .. } => Some(tool_name.clone()),
                _ => None,
            })
            .collect();
        let referenced = messages[idx + 1..].iter().any(|later| {
            if later.role != MessageRole::Assistant {
                return false;
            }
            later.blocks.iter().any(|block| {
                if let ContentBlock::Text { text } = block {
                    tool_names.iter().any(|name| text.contains(name.as_str()))
                } else {
                    false
                }
            })
        });
        let tiny = message.blocks.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { output, .. }
                    if output.chars().count() < SMALL_OUTPUT_PRESERVE_CHARS
            )
        });
        if referenced || tiny {
            protected.insert(idx);
        }
    }
    for idx in &protected {
        summarize_candidates.remove(idx);
    }

    // tool_use_id → input 映射:摘要时带上入参提示,让模型在被压缩后仍能
    // 看出"当时查了什么/做了什么"(如 `[grep_search("分型|脱离") ...]`)。
    let mut input_by_id: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for message in messages {
        for block in &message.blocks {
            if let ContentBlock::ToolUse { id, input, .. } = block {
                input_by_id.insert(id.as_str(), input.as_str());
            }
        }
    }

    messages
        .iter()
        .enumerate()
        .map(|(idx, message)| {
            if !summarize_candidates.contains(&idx) {
                return message.clone();
            }
            let mut new_message = message.clone();
            for block in &mut new_message.blocks {
                let ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } = block
                else {
                    continue;
                };
                // Critical tools (Edit/Write/Delete) and errors are always kept
                // intact, even when old. Already-summarized outputs are left
                // alone to avoid double-summarization.
                if *is_error
                    || is_critical_tool(tool_name)
                    || !is_summarizable_tool(tool_name)
                    || is_already_summarized(output)
                {
                    continue;
                }
                // P0:归档原始 output 到 ToolResultArchive,再替换为摘要。
                archiver(tool_use_id, tool_name, output);
                let input = input_by_id.get(tool_use_id.as_str()).copied().unwrap_or("");
                *output = format_tool_result_summary(tool_name, tool_use_id, input, output);
            }
            new_message
        })
        .collect()
}

fn compacted_summary_prefix_len(session: &Session) -> usize {
    usize::from(
        session
            .messages
            .first()
            .and_then(extract_existing_compacted_summary)
            .is_some(),
    )
}

// ---- 改进点 12: 压缩前提取实验证据 ----

/// 实验证据识别关键词(中英文)。命中任一即视为实验证据。
const EVIDENCE_KEYWORDS: &[&str] = &[
    // 中文关键词
    "对比",
    "基准",
    "矩阵",
    "结果",
    "成功率",
    "失败率",
    "UA",
    "协议",
    // 英文关键词
    "benchmark",
    "comparison",
    "matrix",
    "result",
    "success rate",
    "failure rate",
];

/// 每条证据摘要的最大字符数(含截断标记)。
const EVIDENCE_SUMMARY_MAX_CHARS: usize = 500;

/// 改进点 12:从待压缩的消息中提取实验证据(Heuristic,零 LLM 成本)。
///
/// 识别策略(任一命中即视为实验证据):
/// - **表格数据**:包含 `|` 分隔的多列,且至少 2 行
/// - **数值列表**:连续 ≥ 3 行纯数字(含小数/逗号/正负号)
/// - **关键词**:包含"对比/基准/矩阵/结果/成功率/失败率/UA/协议"等
///
/// 提取摘要:保留前 10 行内容,截断到 500 字符。每条格式为
/// `[tool_name] content`。非 ToolResult 块(如 user/assistant 文本)被跳过。
///
/// 与 `extract_decisions_before_compaction` 互补:decisions 记录"决定了什么",
/// evidence 记录"实验数据是什么"。两者都持久化到 NOTEBOOK.md,跨压缩不丢失。
#[must_use]
pub fn extract_evidence_before_compaction(messages: &[ConversationMessage]) -> Vec<String> {
    let mut evidence = Vec::new();
    for message in messages {
        for block in &message.blocks {
            let ContentBlock::ToolResult {
                tool_name,
                output,
                is_error,
                ..
            } = block
            else {
                continue; // 只从 ToolResult 提取,跳过 Text/Thinking/ToolUse
            };
            // 错误结果不含实验证据
            if *is_error {
                continue;
            }
            if let Some(summary) = extract_evidence_from_output(tool_name, output) {
                evidence.push(summary);
            }
        }
    }
    evidence
}

/// 检查 tool result 输出是否包含实验证据,若有则返回摘要。
fn extract_evidence_from_output(tool_name: &str, output: &str) -> Option<String> {
    let has_table = output
        .lines()
        .filter(|line| line.split('|').count() >= 3)
        .count()
        >= 2;
    let has_numeric_list = output
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && trimmed
                    .chars()
                    .all(|c| c.is_ascii_digit() || ".,".contains(c) || c == ' ' || c == '-')
        })
        .count()
        >= 3;
    let has_keyword = EVIDENCE_KEYWORDS
        .iter()
        .any(|keyword| output.to_lowercase().contains(&keyword.to_lowercase()));

    if !has_table && !has_numeric_list && !has_keyword {
        return None;
    }

    // 提取摘要:前 10 行,截断到 EVIDENCE_SUMMARY_MAX_CHARS(含前缀和截断标记)
    let summary: String = output.lines().take(10).collect::<Vec<_>>().join("\n");
    // 完整输出格式为 `[tool_name] {content}…`,总长度(按字符计)不得超过
    // EVIDENCE_SUMMARY_MAX_CHARS。需预留前缀 `[tool_name] ` 和截断标记 `…`
    // 的空间,避免前缀导致最终长度超限(修复测试 extract_evidence_truncates_to_500_chars)。
    let prefix = format!("[{tool_name}] ");
    let prefix_chars = prefix.chars().count();
    let max_content_chars = EVIDENCE_SUMMARY_MAX_CHARS.saturating_sub(prefix_chars + 1);
    let result = if summary.chars().count() > max_content_chars {
        let truncated: String = summary.chars().take(max_content_chars).collect();
        format!("{prefix}{truncated}…")
    } else {
        format!("{prefix}{summary}")
    };
    Some(result)
}

fn summarize_messages(messages: &[ConversationMessage]) -> String {
    let user_messages = messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .count();
    let assistant_messages = messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .count();
    let tool_messages = messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .count();

    let mut tool_names = messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
            ContentBlock::ToolResult { tool_name, .. } => Some(tool_name.as_str()),
            ContentBlock::Text { .. } | ContentBlock::Thinking { .. } => None,
        })
        .collect::<Vec<_>>();
    tool_names.sort_unstable();
    tool_names.dedup();

    let mut lines = vec![
        "<summary>".to_string(),
        "Conversation summary:".to_string(),
        format!(
            "- Scope: {} earlier messages compacted (user={}, assistant={}, tool={}).",
            messages.len(),
            user_messages,
            assistant_messages,
            tool_messages
        ),
    ];

    if !tool_names.is_empty() {
        lines.push(format!("- Tools mentioned: {}.", tool_names.join(", ")));
    }

    let recent_user_requests = collect_recent_role_summaries(messages, MessageRole::User, 3);
    if !recent_user_requests.is_empty() {
        lines.push("- Recent user requests:".to_string());
        lines.extend(
            recent_user_requests
                .into_iter()
                .map(|request| format!("  - {request}")),
        );
    }

    let pending_work = infer_pending_work(messages);
    if !pending_work.is_empty() {
        lines.push("- Pending work:".to_string());
        lines.extend(pending_work.into_iter().map(|item| format!("  - {item}")));
    }

    let key_files = collect_key_files(messages);
    if !key_files.is_empty() {
        lines.push(format!("- Key files referenced: {}.", key_files.join(", ")));
    }

    if let Some(current_work) = infer_current_work(messages) {
        lines.push(format!("- Current work: {current_work}"));
    }

    lines.push("- Key timeline:".to_string());
    for message in messages {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let content = message
            .blocks
            .iter()
            .map(summarize_block)
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(format!("  - {role}: {content}"));
    }
    lines.push("</summary>".to_string());
    lines.join("\n")
}

fn merge_compact_summaries(existing_summary: Option<&str>, new_summary: &str) -> String {
    let Some(existing_summary) = existing_summary else {
        return new_summary.to_string();
    };

    let previous_highlights = extract_summary_highlights(existing_summary);
    let new_formatted_summary = format_compact_summary(new_summary);
    let new_highlights = extract_summary_highlights(&new_formatted_summary);
    let new_timeline = extract_summary_timeline(&new_formatted_summary);

    let mut lines = vec!["<summary>".to_string(), "Conversation summary:".to_string()];

    // Cap previous highlights to the most recent 9 lines (~3 compaction rounds).
    // Without this cap, highlights accumulate across every compaction and the
    // summary grows unbounded between compress_summary calls.
    const MAX_PREVIOUS_HIGHLIGHT_LINES: usize = 9;
    if !previous_highlights.is_empty() {
        lines.push("- Previously compacted context:".to_string());
        let capped: Vec<String> = if previous_highlights.len() > MAX_PREVIOUS_HIGHLIGHT_LINES {
            previous_highlights[previous_highlights.len() - MAX_PREVIOUS_HIGHLIGHT_LINES..].to_vec()
        } else {
            previous_highlights
        };
        lines.extend(capped.into_iter().map(|line| format!("  {line}")));
    }

    if !new_highlights.is_empty() {
        lines.push("- Newly compacted context:".to_string());
        lines.extend(new_highlights.into_iter().map(|line| format!("  {line}")));
    }

    if !new_timeline.is_empty() {
        lines.push("- Key timeline:".to_string());
        lines.extend(new_timeline.into_iter().map(|line| format!("  {line}")));
    }

    lines.push("</summary>".to_string());
    lines.join("\n")
}

fn summarize_block(block: &ContentBlock) -> String {
    let raw = match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Thinking { thinking, .. } => {
            const MAX_THINKING_SUMMARY_CHARS: usize = 200;
            let trimmed = thinking.trim();
            if trimmed.chars().count() <= MAX_THINKING_SUMMARY_CHARS {
                format!("thinking: {trimmed}")
            } else {
                let truncated: String = trimmed.chars().take(MAX_THINKING_SUMMARY_CHARS).collect();
                format!("thinking: {truncated}…")
            }
        }
        ContentBlock::ToolUse { name, input, .. } => format!("tool_use {name}({input})"),
        ContentBlock::ToolResult {
            tool_name,
            output,
            is_error,
            ..
        } => format!(
            "tool_result {tool_name}: {}{output}",
            if *is_error { "error " } else { "" }
        ),
    };
    truncate_summary(&raw, 160)
}

fn collect_recent_role_summaries(
    messages: &[ConversationMessage],
    role: MessageRole,
    limit: usize,
) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.role == role)
        .rev()
        .filter_map(|message| first_text_block(message))
        .take(limit)
        .map(|text| truncate_summary(text, 160))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn infer_pending_work(messages: &[ConversationMessage]) -> Vec<String> {
    messages
        .iter()
        .rev()
        .filter_map(first_text_block)
        .filter(|text| {
            let lowered = text.to_ascii_lowercase();
            lowered.contains("todo")
                || lowered.contains("next")
                || lowered.contains("pending")
                || lowered.contains("follow up")
                || lowered.contains("remaining")
        })
        .take(3)
        .map(|text| truncate_summary(text, 160))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn collect_key_files(messages: &[ConversationMessage]) -> Vec<String> {
    let mut files = messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .map(|block| match block {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::ToolUse { input, .. } => input.as_str(),
            ContentBlock::ToolResult { output, .. } => output.as_str(),
            ContentBlock::Thinking { thinking, .. } => thinking.as_str(),
        })
        .flat_map(extract_file_candidates)
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files.into_iter().take(8).collect()
}

fn infer_current_work(messages: &[ConversationMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter_map(first_text_block)
        .find(|text| !text.trim().is_empty())
        .map(|text| truncate_summary(text, 200))
}

fn first_text_block(message: &ConversationMessage) -> Option<&str> {
    message.blocks.iter().find_map(|block| match block {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        ContentBlock::ToolUse { .. }
        | ContentBlock::ToolResult { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::Text { .. } => None,
    })
}

fn has_interesting_extension(candidate: &str) -> bool {
    std::path::Path::new(candidate)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["rs", "ts", "tsx", "js", "json", "md"]
                .iter()
                .any(|expected| extension.eq_ignore_ascii_case(expected))
        })
}

fn extract_file_candidates(content: &str) -> Vec<String> {
    content
        .split_whitespace()
        .filter_map(|token| {
            let candidate = token.trim_matches(|char: char| {
                matches!(char, ',' | '.' | ':' | ';' | ')' | '(' | '"' | '\'' | '`')
            });
            if candidate.contains('/') && has_interesting_extension(candidate) {
                Some(candidate.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn truncate_summary(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let mut truncated = content.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
}

fn estimate_message_tokens(message: &ConversationMessage) -> usize {
    // Use char count / 2 instead of byte length / 4 for CJK-aware estimation.
    // CJK characters are 3 bytes in UTF-8 but ~1-2 tokens each, so len()/4
    // severely underestimates CJK content. chars().count()/2 is a better
    // average across mixed CJK/Latin text (CJK: ~1 token/char, Latin: ~0.25
    // token/char, blended average ~0.5 token/char).
    fn char_tokens(s: &str) -> usize {
        s.chars().count() / 2 + 1
    }
    message
        .blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => char_tokens(text),
            ContentBlock::ToolUse { name, input, .. } => char_tokens(name) + char_tokens(input),
            ContentBlock::ToolResult {
                tool_name, output, ..
            } => char_tokens(tool_name) + char_tokens(output),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => char_tokens(thinking) + signature.as_deref().map_or(0, char_tokens),
        })
        .sum()
}

fn extract_tag_block(content: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    let start_index = content.find(&start)? + start.len();
    let end_index = content[start_index..].find(&end)? + start_index;
    Some(content[start_index..end_index].to_string())
}

fn strip_tag_block(content: &str, tag: &str) -> String {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    let mut result = content.to_string();
    while let Some(start_idx) = result.find(&start) {
        let after_start = start_idx + start.len();
        if let Some(end_offset) = result[after_start..].find(&end) {
            let end_idx = after_start + end_offset + end.len();
            result.replace_range(start_idx..end_idx, "");
        } else {
            // No closing tag — remove from start tag to end of string.
            result.replace_range(start_idx.., "");
            break;
        }
    }
    result
}

fn collapse_blank_lines(content: &str) -> String {
    let mut result = String::new();
    let mut last_blank = false;
    for line in content.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && last_blank {
            continue;
        }
        result.push_str(line);
        result.push('\n');
        last_blank = is_blank;
    }
    result
}

fn extract_existing_compacted_summary(message: &ConversationMessage) -> Option<String> {
    if message.role != MessageRole::System {
        return None;
    }

    let text = first_text_block(message)?;
    let text = strip_compact_boundary_marker(text);
    let summary = text.strip_prefix(COMPACT_CONTINUATION_PREAMBLE)?;
    let summary = summary
        .split_once(&format!("\n\n{COMPACT_RECENT_MESSAGES_NOTE}"))
        .map_or(summary, |(value, _)| value);
    let summary = summary
        .split_once(&format!("\n{COMPACT_DIRECT_RESUME_INSTRUCTION}"))
        .map_or(summary, |(value, _)| value);
    Some(summary.trim().to_string())
}

fn extract_summary_highlights(summary: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_timeline = false;

    for line in format_compact_summary(summary).lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed == "Summary:" || trimmed == "Conversation summary:" {
            continue;
        }
        if trimmed == "- Key timeline:" {
            in_timeline = true;
            continue;
        }
        if in_timeline {
            continue;
        }
        lines.push(trimmed.to_string());
    }

    lines
}

fn extract_summary_timeline(summary: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_timeline = false;

    for line in format_compact_summary(summary).lines() {
        let trimmed = line.trim_end();
        if trimmed == "- Key timeline:" {
            in_timeline = true;
            continue;
        }
        if !in_timeline {
            continue;
        }
        if trimmed.is_empty() {
            break;
        }
        lines.push(trimmed.to_string());
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::{
        collect_key_files, compact_session, compact_session_with_trigger, estimate_message_tokens,
        extract_compact_boundary, format_compact_summary, format_tool_result_summary,
        get_compact_continuation_message, get_messages_after_compact_boundary, infer_pending_work,
        is_already_summarized, merge_compact_summaries, microcompact, should_compact,
        CompactBoundary, CompactTrigger, CompactionConfig, CompactionSummarizerClient,
    };
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};

    #[test]
    fn formats_compact_summary_like_upstream() {
        let summary = "<analysis>scratch</analysis>\n<summary>Kept work</summary>";
        assert_eq!(format_compact_summary(summary), "Summary:\nKept work");
    }

    #[test]
    fn leaves_small_sessions_unchanged() {
        let mut session = Session::new();
        session.messages = vec![ConversationMessage::user_text("hello")];

        let result = compact_session(&session, CompactionConfig::default());
        assert_eq!(result.removed_message_count, 0);
        assert_eq!(result.compacted_session, session);
        assert!(result.summary.is_empty());
        assert!(result.formatted_summary.is_empty());
    }

    /// 假 client — 返回固定摘要;可配置 Err 模拟故障。
    struct FakeSummarizer {
        result: Result<String, String>,
    }
    impl CompactionSummarizerClient for FakeSummarizer {
        fn summarize(&self, _prompt: &str) -> Result<String, String> {
            self.result.clone()
        }
    }

    #[test]
    fn llm_summarizer_used_when_client_present() {
        // 注入 fake client 后,摘要应来自 LLM(含 fake 文本),
        // 而非启发式(不含 "Scope:" 统计行)。
        let messages = vec![
            ConversationMessage::user_text("please refactor the auth module"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "I will split it into two files".to_string(),
            }]),
        ];
        let client = FakeSummarizer {
            result: Ok("- implemented auth split
- pending: run tests"
                .to_string()),
        };
        let summary = super::summarize_messages_with_client(&messages, Some(&client));
        let rendered = super::format_compact_summary(&summary);
        assert!(
            rendered.contains("implemented auth split"),
            "LLM summary should be used: {rendered}"
        );
        assert!(
            !rendered.contains("Scope:"),
            "heuristic summary should NOT be used: {rendered}"
        );
        // 规范化为 bullet,merge 阶段能提取。
        let highlights = super::extract_summary_highlights(&summary);
        assert!(highlights.iter().any(|h| h.contains("pending: run tests")));
    }

    #[test]
    fn llm_summarizer_falls_back_to_heuristic_on_error() {
        let messages = vec![
            ConversationMessage::user_text("please refactor the auth module"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "I will split it into two files".to_string(),
            }]),
        ];
        let client = FakeSummarizer {
            result: Err("api down".to_string()),
        };
        let summary = super::summarize_messages_with_client(&messages, Some(&client));
        // 失败回退启发式 — 含 "Scope:" 统计行。
        assert!(
            summary.contains("Scope:"),
            "should fall back to heuristic: {summary}"
        );
    }

    #[test]
    fn llm_summarizer_falls_back_to_heuristic_on_empty_output() {
        let messages = vec![ConversationMessage::user_text("hi")];
        let client = FakeSummarizer {
            result: Ok("   
  "
            .to_string()),
        };
        let summary = super::summarize_messages_with_client(&messages, Some(&client));
        assert!(
            summary.contains("Scope:"),
            "empty LLM output should fall back: {summary}"
        );
    }

    #[test]
    fn sanitize_llm_summary_normalizes_freeform_text_to_bullets() {
        let out = super::sanitize_llm_summary(
            "plain line one
- already bullet

second para",
        );
        let highlights = super::extract_summary_highlights(&out);
        assert_eq!(highlights.len(), 3);
        assert!(highlights[0].contains("plain line one"));
        assert!(highlights[1].contains("already bullet"));
        assert!(highlights[2].contains("second para"));
    }

    #[test]
    fn llm_summarize_prompt_contains_transcript_and_instructions() {
        let messages = vec![ConversationMessage::user_text("fix the bug in parser.rs")];
        let prompt = super::build_llm_summarize_prompt(&messages);
        assert!(
            prompt.contains("parser.rs"),
            "transcript should be in prompt"
        );
        assert!(prompt.contains("session-compaction summarizer"));
        assert!(prompt.contains("- "), "instructions should mention bullets");
    }

    #[test]
    fn compacts_older_messages_into_a_system_summary() {
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::tool_result("1", "bash", "ok ".repeat(200), false),
            ConversationMessage {
                role: MessageRole::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "recent".to_string(),
                }],
                usage: None,
            },
        ];

        let result = compact_session(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            },
        );

        // With the tool-use/tool-result boundary fix, the compaction preserves
        // one extra message to avoid an orphaned tool result at the boundary.
        // messages[1] (assistant) must be kept along with messages[2] (tool result).
        assert!(
            result.removed_message_count <= 2,
            "expected at most 2 removed, got {}",
            result.removed_message_count
        );
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert!(matches!(
            &result.compacted_session.messages[0].blocks[0],
            ContentBlock::Text { text } if text.contains("Summary:")
        ));
        assert!(result.formatted_summary.contains("Scope:"));
        assert!(result.formatted_summary.contains("Key timeline:"));
        assert!(should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            }
        ));
        // Note: with the tool-use/tool-result boundary guard the compacted session
        // may preserve one extra message at the boundary, so token reduction is
        // not guaranteed for small sessions. The invariant that matters is that
        // the removed_message_count is non-zero (something was compacted).
        assert!(
            result.removed_message_count > 0,
            "compaction must remove at least one message"
        );
    }

    #[test]
    fn keeps_previous_compacted_context_when_compacting_again() {
        let mut initial_session = Session::new();
        initial_session.messages = vec![
            ConversationMessage::user_text("Investigate rust/crates/runtime/src/compact.rs"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "I will inspect the compact flow.".to_string(),
            }]),
            ConversationMessage::user_text("Also update rust/crates/runtime/src/conversation.rs"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Next: preserve prior summary context during auto compact.".to_string(),
            }]),
        ];
        let config = CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let first = compact_session(&initial_session, config);
        let mut follow_up_messages = first.compacted_session.messages.clone();
        follow_up_messages.extend([
            ConversationMessage::user_text("Please add regression tests for compaction."),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Working on regression coverage now.".to_string(),
            }]),
        ]);

        let mut second_session = Session::new();
        second_session.messages = follow_up_messages;
        let second = compact_session(&second_session, config);

        assert!(second
            .formatted_summary
            .contains("Previously compacted context:"));
        assert!(second
            .formatted_summary
            .contains("Scope: 2 earlier messages compacted"));
        assert!(second
            .formatted_summary
            .contains("Newly compacted context:"));
        assert!(second
            .formatted_summary
            .contains("Also update rust/crates/runtime/src/conversation.rs"));
        assert!(matches!(
            &second.compacted_session.messages[0].blocks[0],
            ContentBlock::Text { text }
                if text.contains("Previously compacted context:")
                    && text.contains("Newly compacted context:")
        ));
        assert!(matches!(
            &second.compacted_session.messages[1].blocks[0],
            ContentBlock::Text { text } if text.contains("Please add regression tests for compaction.")
        ));
    }

    #[test]
    fn ignores_existing_compacted_summary_when_deciding_to_recompact() {
        let summary = "<summary>Conversation summary:\n- Scope: earlier work preserved.\n- Key timeline:\n  - user: large preserved context\n</summary>";
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage {
                role: MessageRole::System,
                blocks: vec![ContentBlock::Text {
                    text: get_compact_continuation_message(summary, true, true),
                }],
                usage: None,
            },
            ConversationMessage::user_text("tiny"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "recent".to_string(),
            }]),
        ];

        assert!(!should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            }
        ));
    }

    #[test]
    fn truncates_long_blocks_in_summary() {
        let summary = super::summarize_block(&ContentBlock::Text {
            text: "x".repeat(400),
        });
        assert!(summary.ends_with('…'));
        assert!(summary.chars().count() <= 161);
    }

    #[test]
    fn extracts_key_files_from_message_content() {
        let files = collect_key_files(&[ConversationMessage::user_text(
            "Update rust/crates/runtime/src/compact.rs and rust/crates/rusty-claude-cli/src/main.rs next.",
        )]);
        assert!(files.contains(&"rust/crates/runtime/src/compact.rs".to_string()));
        assert!(files.contains(&"rust/crates/rusty-claude-cli/src/main.rs".to_string()));
    }

    /// Regression: compaction must not split an assistant(ToolUse) /
    /// user(ToolResult) pair at the boundary. An orphaned tool-result message
    /// without the preceding assistant `tool_calls` causes a 400 on the
    /// OpenAI-compat path (internal repro 2026-04-09).
    #[test]
    fn compaction_does_not_split_tool_use_tool_result_pair() {
        use crate::session::{ContentBlock, Session};

        let tool_id = "call_abc";
        let mut session = Session::default();
        // Turn 1: user prompt
        session
            .push_message(ConversationMessage::user_text("Search for files"))
            .unwrap();
        // Turn 2: assistant calls a tool
        session
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::ToolUse {
                    id: tool_id.to_string(),
                    name: "search".to_string(),
                    input: "{\"q\":\"*.rs\"}".to_string(),
                },
            ]))
            .unwrap();
        // Turn 3: tool result
        session
            .push_message(ConversationMessage::tool_result(
                tool_id,
                "search",
                "found 5 files",
                false,
            ))
            .unwrap();
        // Turn 4: assistant final response
        session
            .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Done.".to_string(),
            }]))
            .unwrap();

        // Compact preserving only 1 recent message — without the fix this
        // would cut the boundary so that the tool result (turn 3) is first,
        // without its preceding assistant tool_calls (turn 2).
        let config = CompactionConfig {
            preserve_recent_messages: 1,
            ..CompactionConfig::default()
        };
        let result = compact_session(&session, config);
        // After compaction, no two consecutive messages should have the pattern
        // tool_result immediately following a non-assistant message (i.e. an
        // orphaned tool result without a preceding assistant ToolUse).
        let messages = &result.compacted_session.messages;
        for i in 1..messages.len() {
            let curr_is_tool_result = messages[i]
                .blocks
                .first()
                .is_some_and(|b| matches!(b, ContentBlock::ToolResult { .. }));
            if curr_is_tool_result {
                let prev_has_tool_use = messages[i - 1]
                    .blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
                assert!(
                    prev_has_tool_use,
                    "message[{}] is a ToolResult but message[{}] has no ToolUse: {:?}",
                    i,
                    i - 1,
                    &messages[i - 1].blocks
                );
            }
        }
    }

    #[test]
    fn infers_pending_work_from_recent_messages() {
        let pending = infer_pending_work(&[
            ConversationMessage::user_text("done"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Next: update tests and follow up on remaining CLI polish.".to_string(),
            }]),
        ]);
        assert_eq!(pending.len(), 1);
        assert!(pending[0].contains("Next: update tests"));
    }

    #[test]
    fn compact_session_summary_is_compressed_to_budget() {
        // Build a session with many long messages so summarize_messages produces
        // a summary exceeding the default compression budget (1200 chars).
        let mut session = Session::new();
        for i in 0..50 {
            let long_text = format!(
                "User message number {i} with a very long body. {}",
                "x".repeat(200)
            );
            session.push_user_text(long_text).unwrap();
            let assistant_msg = ConversationMessage {
                role: MessageRole::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: format!(
                        "Assistant response {i} with substantial content. {}",
                        "y".repeat(200)
                    ),
                }],
                usage: None,
            };
            session.push_message(assistant_msg).unwrap();
        }

        let config = CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 0, // force compaction
        };
        let result = compact_session(&session, config);
        // compress_summary_text default budget is 1200 chars / 24 lines.
        // The compressed summary should be within budget (allowing some slack
        // for the continuation message wrapper).
        assert!(
            result.summary.chars().count() <= 2_000,
            "summary should be compressed, got {} chars",
            result.summary.chars().count()
        );
        assert!(
            result.summary.lines().count() <= 30,
            "summary should have bounded lines, got {} lines",
            result.summary.lines().count()
        );
    }

    #[test]
    fn merge_compact_summaries_caps_previous_highlights() {
        // Build an existing summary with more than 9 highlight lines.
        let mut existing_lines = vec![
            "<summary>".to_string(),
            "Conversation summary:".to_string(),
            "- Previously compacted context:".to_string(),
        ];
        for i in 0..20 {
            existing_lines.push(format!("  highlight line {i}"));
        }
        existing_lines.push("</summary>".to_string());
        let existing = existing_lines.join("\n");

        let new_summary = "new content";
        let merged = merge_compact_summaries(Some(&existing), new_summary);

        // Count "highlight line" occurrences in the Previously compacted section.
        let previously_section = merged
            .split("- Previously compacted context:")
            .nth(1)
            .and_then(|s| s.split("- Newly compacted context:").next())
            .unwrap_or("");
        let highlight_count = previously_section
            .lines()
            .filter(|l| l.contains("highlight line"))
            .count();
        assert!(
            highlight_count <= 9,
            "previous highlights should be capped at 9, got {highlight_count}"
        );
        // The most recent highlights should be retained (lines 11-19, i.e., 9 lines)
        assert!(
            previously_section.contains("highlight line 19"),
            "most recent highlight should be retained"
        );
        assert!(
            !previously_section.contains("highlight line 0"),
            "oldest highlight should be dropped"
        );
    }

    #[test]
    fn estimate_message_tokens_is_cjk_aware() {
        // CJK text: 12 chars, 36 bytes UTF-8.
        // Old estimate: 36 / 4 + 1 = 10 tokens (overestimate by byte length).
        // New estimate: 12 chars / 2 + 1 = 7 tokens (closer to ~12-24 actual).
        let cjk_text = "你好世界，这是一个测试。";
        let message = ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text {
                text: cjk_text.to_string(),
            }],
            usage: None,
        };
        let estimated = estimate_message_tokens(&message);
        // chars().count() / 2 + 1 for 12 chars = 7
        assert!(
            (5..=8).contains(&estimated),
            "CJK estimate should be in reasonable range, got {estimated}"
        );
        // ASCII of similar char count should be comparable, not wildly different.
        let ascii_text = "hello world!"; // 12 chars, 12 bytes
        let ascii_msg = ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text {
                text: ascii_text.to_string(),
            }],
            usage: None,
        };
        let ascii_estimated = estimate_message_tokens(&ascii_msg);
        // ASCII: 12/2+1 = 7; CJK: 12/2+1 = 7 — comparable, as they should be.
        // Old code would give CJK 36/4+1=10 vs ASCII 12/4+1=4, wrongly implying
        // CJK uses far more tokens per char than ASCII.
        assert!(ascii_estimated >= 5);
    }

    #[test]
    fn summarize_block_preserves_thinking_content() {
        let block = ContentBlock::Thinking {
            thinking: "I should consider the edge case where input is empty.".to_string(),
            signature: None,
        };
        let summary = super::summarize_block(&block);
        assert!(
            summary.contains("consider the edge case"),
            "thinking content should be preserved in summary, got: {summary}"
        );
        assert!(
            !summary.contains("chars"),
            "should not use char count placeholder, got: {summary}"
        );
    }

    #[test]
    fn summarize_block_truncates_long_thinking() {
        let long_thinking = "x".repeat(500);
        let block = ContentBlock::Thinking {
            thinking: long_thinking,
            signature: None,
        };
        let summary = super::summarize_block(&block);
        // Should be truncated, not 500+ chars
        assert!(
            summary.chars().count() < 300,
            "long thinking should be truncated, got {} chars",
            summary.chars().count()
        );
    }

    #[test]
    fn strip_tag_block_removes_all_occurrences() {
        let content = "before <foo>first</foo> middle <foo>second</foo> after";
        let result = super::strip_tag_block(content, "foo");
        assert_eq!(result, "before  middle  after");
    }

    #[test]
    fn strip_tag_block_handles_single_occurrence() {
        let content = "text <bar>block</bar> end";
        let result = super::strip_tag_block(content, "bar");
        assert_eq!(result, "text  end");
    }

    #[test]
    fn strip_tag_block_handles_no_match() {
        let content = "no tags here";
        let result = super::strip_tag_block(content, "foo");
        assert_eq!(result, "no tags here");
    }

    #[test]
    fn strip_tag_block_handles_unclosed_tag() {
        let content = "text <foo>unclosed rest";
        let result = super::strip_tag_block(content, "foo");
        assert_eq!(result, "text ");
    }

    // ---- P0-2: Compact Boundary tests ----

    #[test]
    fn compact_boundary_marker_inserted_after_compaction() {
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::tool_result("1", "bash", "ok ".repeat(200), false),
            ConversationMessage {
                role: MessageRole::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "recent".to_string(),
                }],
                usage: None,
            },
        ];

        let result = compact_session(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            },
        );

        assert!(
            result.removed_message_count > 0,
            "compaction must remove at least one message"
        );
        let system_message = &result.compacted_session.messages[0];
        assert_eq!(system_message.role, MessageRole::System);
        let ContentBlock::Text { text } = &system_message.blocks[0] else {
            panic!("expected text block in system message");
        };
        assert!(
            text.contains("<!-- compact_boundary:"),
            "system message must contain boundary marker, got: {text}"
        );

        let boundary = extract_compact_boundary(&result.compacted_session.messages)
            .expect("boundary should be parseable from compacted messages");
        assert_eq!(boundary.trigger, CompactTrigger::Auto);
        assert_eq!(boundary.messages_summarized, result.removed_message_count);
        assert!(boundary.pre_tokens > 0);
        assert!(boundary.timestamp_ms > 0);
    }

    #[test]
    fn compact_boundary_marker_carries_reactive_trigger() {
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::tool_result("1", "bash", "ok ".repeat(200), false),
            ConversationMessage {
                role: MessageRole::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "recent".to_string(),
                }],
                usage: None,
            },
        ];

        let result = compact_session_with_trigger(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            },
            CompactTrigger::Reactive,
        );

        let boundary = extract_compact_boundary(&result.compacted_session.messages)
            .expect("boundary should be present");
        assert_eq!(boundary.trigger, CompactTrigger::Reactive);
    }

    #[test]
    fn get_messages_after_compact_boundary_slices_correctly() {
        // Build a message list with a boundary marker in the middle.
        let stale_user = ConversationMessage::user_text("stale before compaction");
        let stale_assistant = ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "stale response".to_string(),
        }]);
        let boundary_text = "This session is being continued from a previous conversation.\n<!-- compact_boundary: {\"trigger\":\"auto\",\"pre_tokens\":1000,\"messages_summarized\":2,\"timestamp_ms\":1700000000000} -->".to_string();
        let boundary_system = ConversationMessage {
            role: MessageRole::System,
            blocks: vec![ContentBlock::Text {
                text: boundary_text,
            }],
            usage: None,
        };
        let fresh_user = ConversationMessage::user_text("fresh question");
        let fresh_assistant = ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "fresh response".to_string(),
        }]);

        let messages = vec![
            stale_user,
            stale_assistant,
            boundary_system,
            fresh_user,
            fresh_assistant,
        ];

        let sliced = get_messages_after_compact_boundary(&messages);
        // The slice should start at the boundary system message (index 2) and
        // include everything after it.
        assert_eq!(sliced.len(), 3);
        assert_eq!(sliced[0].role, MessageRole::System);
        assert!(matches!(
            &sliced[1].blocks[0],
            ContentBlock::Text { text } if text == "fresh question"
        ));
        assert!(matches!(
            &sliced[2].blocks[0],
            ContentBlock::Text { text } if text == "fresh response"
        ));
    }

    #[test]
    fn get_messages_after_compact_boundary_uses_most_recent_boundary() {
        // When multiple boundaries exist, the most recent one wins.
        let old_boundary = ConversationMessage {
            role: MessageRole::System,
            blocks: vec![ContentBlock::Text {
                text: "old summary\n<!-- compact_boundary: {\"trigger\":\"auto\",\"pre_tokens\":500,\"messages_summarized\":1,\"timestamp_ms\":1} -->".to_string(),
            }],
            usage: None,
        };
        let middle_user = ConversationMessage::user_text("middle");
        let new_boundary = ConversationMessage {
            role: MessageRole::System,
            blocks: vec![ContentBlock::Text {
                text: "new summary\n<!-- compact_boundary: {\"trigger\":\"auto\",\"pre_tokens\":800,\"messages_summarized\":2,\"timestamp_ms\":2} -->".to_string(),
            }],
            usage: None,
        };
        let fresh_user = ConversationMessage::user_text("fresh");

        let messages = vec![old_boundary, middle_user, new_boundary, fresh_user];
        let sliced = get_messages_after_compact_boundary(&messages);
        assert_eq!(sliced.len(), 2);
        assert_eq!(sliced[0].role, MessageRole::System);
        assert!(matches!(
            &sliced[1].blocks[0],
            ContentBlock::Text { text } if text == "fresh"
        ));
    }

    #[test]
    fn get_messages_after_compact_boundary_returns_all_when_no_boundary() {
        let messages = vec![
            ConversationMessage::user_text("hello"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "hi".to_string(),
            }]),
        ];

        let sliced = get_messages_after_compact_boundary(&messages);
        assert_eq!(sliced.len(), messages.len());
        assert!(std::ptr::eq(sliced.as_ptr(), messages.as_ptr()));
    }

    #[test]
    fn extract_compact_boundary_returns_none_when_absent() {
        let messages = vec![ConversationMessage::user_text("no boundary here")];
        assert!(extract_compact_boundary(&messages).is_none());
    }

    #[test]
    fn extract_existing_summary_strips_boundary_marker() {
        // When compacting a session whose existing system message already has
        // a boundary marker, the extracted summary must not include the marker.
        let mut session = Session::new();
        let first_summary = "earlier work summary";
        let continuation = super::get_compact_continuation_message(first_summary, true, true);
        let boundary = CompactBoundary {
            trigger: CompactTrigger::Auto,
            pre_tokens: 500,
            messages_summarized: 3,
            timestamp_ms: 1,
        };
        let marked = format!(
            "{continuation}\n{}",
            super::format_compact_boundary_marker(&boundary)
        );
        session.messages = vec![
            ConversationMessage {
                role: MessageRole::System,
                blocks: vec![ContentBlock::Text { text: marked }],
                usage: None,
            },
            ConversationMessage::user_text("follow up"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "response".to_string(),
            }]),
        ];

        let extracted = session
            .messages
            .first()
            .and_then(super::extract_existing_compacted_summary)
            .expect("summary should extract");
        assert!(
            !extracted.contains("compact_boundary"),
            "extracted summary must not contain boundary marker: {extracted}"
        );
        assert!(
            extracted.contains("earlier work summary"),
            "extracted summary should contain the summary text: {extracted}"
        );
    }

    // ---- P1-4: Microcompact tests ----

    #[test]
    fn microcompact_preserves_recent_tool_results() {
        // 旧结果输出需超过 SMALL_OUTPUT_PRESERVE_CHARS,否则被小输出保护保留
        let long_output = "line1: file header content\nline2: import statements block\nline3: function start signature\nline4: function body implementation detail\nline5: function end marker\nline6: additional padding line that extends the output well beyond two hundred characters to satisfy the small output preservation threshold used by microcompact\nline7: more padding\nline8: final padding to be safe";
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "Read", long_output, false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Read", "recent1\nrecent2", false),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("3", "Read", "newest\nnewest2", false),
        ];

        let result = microcompact(&messages, 2);
        // The last 2 tool results must be preserved verbatim.
        let tool_results: Vec<_> = result
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 3);
        // Oldest one (index 0) should be summarized.
        assert!(
            tool_results[0].contains("summarized"),
            "oldest tool result should be summarized, got: {}",
            tool_results[0]
        );
        // Recent two should be intact.
        assert_eq!(tool_results[1], "recent1\nrecent2");
        assert_eq!(tool_results[2], "newest\nnewest2");
    }

    #[test]
    fn microcompact_summarizes_old_read_results() {
        // 旧结果输出需超过 SMALL_OUTPUT_PRESERVE_CHARS,否则被小输出保护保留
        let long_output = "file contents line 1: package declaration and module imports\nfile contents line 2: struct definition with several fields spanning long names\nfile contents line 3: function signature with generic bounds and lifetimes that make this line long enough\nfile contents line 4: function body with detailed implementation comments\nfile contents line 5: closing brace and module end marker with trailing newline padding that extends beyond the two hundred character threshold used by microcompact's small output preservation";
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "Read", long_output, false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Read", "recent", false),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("3", "Read", "newest", false),
            ConversationMessage::user_text("q4"),
            ConversationMessage::tool_result("4", "Read", "newest2", false),
        ];

        let result = microcompact(&messages, 2);
        let old_tool_result = &result[1].blocks[0];
        let ContentBlock::ToolResult {
            tool_name,
            output,
            is_error,
            ..
        } = old_tool_result
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_name, "Read");
        assert!(!*is_error);
        assert!(
            output.contains("[Read ") && output.contains(" summarized:"),
            "old Read result should be summarized, got: {output}"
        );
        assert!(
            output.contains("chars →"),
            "summary should include char count and first line, got: {output}"
        );
        assert!(
            output.contains("file contents line 1"),
            "summary should include first line, got: {output}"
        );
    }

    #[test]
    fn microcompact_preserves_referenced_tool_results() {
        // 依赖感知保护(Self-GC arXiv:2607.00692):旧 tool result 被后续 assistant
        // 文本提及工具名时,即使超出保留窗口也不得摘要 —— 它是当前讨论引用的
        // 活跃证据,压掉后模型只能重新调用工具查询(重复劳动 + token 浪费)。
        let long_output = "line1: analysis result detail\nline2: engine reproduction shows 6 items in total\nline3: breakdown per category with long names and annotations that push this output well beyond two hundred characters of length so that the small output preservation rule does not kick in and mask the reference protection logic under test\nline4: more detail rows\nline5: trailing padding line";
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "Bash", long_output, false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "根据 Bash 输出,引擎复现是 6 笔".to_string(),
            }]),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("2", "Read", "recent", false),
        ];

        // preserve_recent=1:只有最后一个 Read 在保留窗口内;旧的 Bash 本应被
        // 摘要,但因被后续 assistant 文本引用而必须保留原文。
        let result = microcompact(&messages, 1);
        let ContentBlock::ToolResult { output, .. } = &result[1].blocks[0] else {
            panic!("expected tool result");
        };
        assert!(
            output.contains("engine reproduction shows 6 items"),
            "referenced tool result must keep full output, got: {output}"
        );
        // 窗口内结果照常保留(对照)。
        let ContentBlock::ToolResult { output, .. } = &result[5].blocks[0] else {
            panic!("expected tool result");
        };
        assert_eq!(output, "recent");
    }

    #[test]
    fn microcompact_summary_keeps_input_hint() {
        // 摘要带 input 提示:被压缩后模型仍能看出"当时查了什么/做了什么",
        // 避免因看不到搜索词/文件路径而重新调用工具查询。
        let long_output = "line1: file header\nline2: module imports and package declarations that are quite verbose and push this output far beyond two hundred characters in order to avoid the small output preservation branch and exercise the input hint path in the summarizer\nline3: struct definitions\nline4: function implementations with comments\nline5: trailing content to guarantee sufficient length";
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "1".to_string(),
                name: "Read".to_string(),
                input: r#"{"file_path": "src/main.rs"}"#.to_string(),
            }]),
            ConversationMessage::tool_result("1", "Read", long_output, false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Read", "recent", false),
        ];

        let result = microcompact(&messages, 1);
        let ContentBlock::ToolResult { output, .. } = &result[2].blocks[0] else {
            panic!("expected tool result");
        };
        assert!(
            output.contains("[Read(src/main.rs)"),
            "summary should carry input hint, got: {output}"
        );
    }

    #[test]
    fn microcompact_keeps_edit_write_results_intact() {
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result(
                "1",
                "Edit",
                "The file has been updated successfully.",
                false,
            ),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Write", "File written.", false),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("3", "Read", "recent read", false),
        ];

        // preserve_recent=1 means only the last tool result is "recent".
        // The Edit and Write results are old but must remain intact.
        let result = microcompact(&messages, 1);
        let edit_result = &result[1].blocks[0];
        let ContentBlock::ToolResult { output, .. } = edit_result else {
            panic!("expected edit tool result");
        };
        assert_eq!(
            *output, "The file has been updated successfully.",
            "Edit result must be preserved verbatim even when old"
        );

        let write_result = &result[3].blocks[0];
        let ContentBlock::ToolResult { output, .. } = write_result else {
            panic!("expected write tool result");
        };
        assert_eq!(
            *output, "File written.",
            "Write result must be preserved verbatim even when old"
        );
    }

    #[test]
    fn microcompact_preserves_error_results() {
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "Read", "Error: file not found", true),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Read", "recent", false),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("3", "Read", "newest", false),
        ];

        // preserve_recent=1: only the last Read is recent.
        // The error result (index 1) is old but must be preserved.
        let result = microcompact(&messages, 1);
        let error_result = &result[1].blocks[0];
        let ContentBlock::ToolResult {
            output, is_error, ..
        } = error_result
        else {
            panic!("expected tool result");
        };
        assert!(
            *is_error,
            "error flag must be preserved on old error results"
        );
        assert_eq!(
            *output, "Error: file not found",
            "error tool result must be preserved verbatim even when old"
        );
    }

    #[test]
    fn microcompact_does_not_double_summarize() {
        let already_summarized = "[Read output summarized: 100 chars → first line…]";
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "Read", already_summarized, false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Read", "recent", false),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("3", "Read", "newest", false),
        ];

        let result = microcompact(&messages, 2);
        let old_result = &result[1].blocks[0];
        let ContentBlock::ToolResult { output, .. } = old_result else {
            panic!("expected tool result");
        };
        assert_eq!(
            *output, already_summarized,
            "already-summarized output should not be re-summarized"
        );
    }

    #[test]
    fn microcompact_preserves_bash_and_grep_results_when_recent() {
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "Bash", "command output", false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Grep", "grep match", false),
        ];

        // preserve_recent=2: both are recent, neither should be summarized.
        let result = microcompact(&messages, 2);
        let bash_output = match &result[1].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => panic!("expected bash result"),
        };
        assert_eq!(bash_output, "command output");
        let grep_output = match &result[3].blocks[0] {
            ContentBlock::ToolResult { output, .. } => output.clone(),
            _ => panic!("expected grep result"),
        };
        assert_eq!(grep_output, "grep match");
    }

    #[test]
    fn microcompact_returns_unchanged_when_no_tool_results() {
        let messages = vec![
            ConversationMessage::user_text("hello"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "hi".to_string(),
            }]),
        ];

        let result = microcompact(&messages, 4);
        assert_eq!(result, messages);
    }

    /// P1:dispatch_subagent 结果必须保留完整(关键指针:subagent_id + result_ref)。
    ///
    /// 子智能体结果包含主 agent 后续读取所需的 result_ref 路径,
    /// 即使很老也不能被摘要丢失。
    #[test]
    fn microcompact_preserves_dispatch_subagent_results() {
        let dispatch_output =
            "Subagent `subagent-1` completed. Result written to: .claw/subagents/subagent-1.md\n\
             Use Read tool to inspect the result. The subagent ran with an isolated context.";
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "dispatch_subagent", dispatch_output, false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Read", "recent read", false),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("3", "Read", "newest read", false),
        ];

        // preserve_recent=2: 只有最后 2 条 tool result 是 recent。
        // dispatch_subagent 是最老的,但因 P1 加入 CRITICAL_TOOLS,必须保留完整。
        let result = microcompact(&messages, 2);
        let dispatch_result = &result[1].blocks[0];
        let ContentBlock::ToolResult {
            tool_name,
            output,
            is_error,
            ..
        } = dispatch_result
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_name, "dispatch_subagent");
        assert!(!*is_error);
        assert_eq!(
            *output, dispatch_output,
            "P1: dispatch_subagent result must be preserved verbatim (contains critical result_ref pointer)"
        );
        assert!(
            output.contains(".claw/subagents/subagent-1.md"),
            "P1: result_ref path must be preserved: {output}"
        );
    }

    /// P1:check_subagent 结果必须保留完整(关键指针:subagent_id + status + result)。
    #[test]
    fn microcompact_preserves_check_subagent_results() {
        let check_output = r#"{
  "subagent_id": "subagent-2",
  "status": "completed",
  "terminal": true,
  "result": ".claw/subagents/subagent-2.md",
  "name": "analyzer",
  "mode": "fork"
}"#;
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "check_subagent", check_output, false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Read", "recent", false),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("3", "Read", "newest", false),
        ];

        let result = microcompact(&messages, 2);
        let check_result = &result[1].blocks[0];
        let ContentBlock::ToolResult {
            tool_name, output, ..
        } = check_result
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_name, "check_subagent");
        assert_eq!(
            *output, check_output,
            "P1: check_subagent result must be preserved verbatim (contains critical status/result pointer)"
        );
    }

    /// P1:format_tool_result_summary 从"只保留第一行"改为"保留前 N 行 + 行数信息"。
    ///
    /// 验证多行 Read 结果的摘要包含前 3 行 + 总行数提示,
    /// 而非旧格式只包含第一行。
    #[test]
    fn format_tool_result_summary_preserves_multiple_lines() {
        let multi_line_output = "line1: file header\nline2: import statement\nline3: function start\nline4: function body\nline5: function end";
        let summary = format_tool_result_summary("Read", "call_test1", "", multi_line_output);

        // 应包含前 3 行(不是只第一行)
        assert!(
            summary.contains("line1: file header"),
            "summary should contain line1: {summary}"
        );
        assert!(
            summary.contains("line2: import statement"),
            "P1: summary should contain line2 (multi-line preview): {summary}"
        );
        assert!(
            summary.contains("line3: function start"),
            "P1: summary should contain line3 (multi-line preview): {summary}"
        );
        // 第 4、5 行不应在 preview 中(只保留前 3 行)
        assert!(
            !summary.contains("line4: function body"),
            "summary should not contain line4 (beyond MAX_PREVIEW_LINES=3): {summary}"
        );
        // 应包含总行数提示(5 lines total)
        assert!(
            summary.contains("5 lines total"),
            "P1: summary should include total line count: {summary}"
        );
        // 应包含原字符数
        assert!(
            summary.contains("chars →"),
            "summary should include char count: {summary}"
        );
        // 应以 …] 结尾(保持 is_already_summarized 兼容)
        assert!(
            summary.ends_with("…]"),
            "summary should end with '…]' for is_already_summarized compatibility: {summary}"
        );
    }

    /// P1:验证 is_already_summarized 对新格式仍然有效(避免重复摘要)。
    #[test]
    fn is_already_summarized_recognizes_new_multi_line_format() {
        let new_format = format_tool_result_summary(
            "Read",
            "call_test2",
            "",
            "line1\nline2\nline3\nline4\nline5",
        );
        assert!(
            is_already_summarized(&new_format),
            "P1: is_already_summarized should recognize new multi-line format: {new_format}"
        );

        // 验证新格式不会被重复摘要
        let messages = vec![
            ConversationMessage::user_text("q1"),
            ConversationMessage::tool_result("1", "Read", new_format.clone(), false),
            ConversationMessage::user_text("q2"),
            ConversationMessage::tool_result("2", "Read", "recent", false),
            ConversationMessage::user_text("q3"),
            ConversationMessage::tool_result("3", "Read", "newest", false),
        ];

        let result = microcompact(&messages, 2);
        let old_result = &result[1].blocks[0];
        let ContentBlock::ToolResult { output, .. } = old_result else {
            panic!("expected tool result");
        };
        assert_eq!(
            *output, new_format,
            "P1: new-format summary should not be re-summarized (is_already_summarized must return true)"
        );
    }

    /// P1:短输出(≤ 3 行)的摘要不应附加 "(N lines total)" 提示。
    #[test]
    fn format_tool_result_summary_omits_line_count_for_short_output() {
        let short_output = "only one line here";
        let summary = format_tool_result_summary("Read", "call_test3", "", short_output);
        assert!(
            summary.contains("only one line here"),
            "summary should contain the single line: {summary}"
        );
        assert!(
            !summary.contains("lines total"),
            "P1: short output (≤3 lines) should not include 'lines total' hint: {summary}"
        );
    }

    // ---- 改进点 12: extract_evidence_before_compaction ----

    #[test]
    fn extract_evidence_detects_table_data() {
        let messages = vec![ConversationMessage::tool_result(
            "call_1",
            "Bash",
            "| 方案 | 成功率 | 延迟 |\n|------|--------|------|\n| A    | 95%    | 10ms |\n| B    | 88%    | 20ms |",
            false,
        )];
        let evidence = super::extract_evidence_before_compaction(&messages);
        assert_eq!(evidence.len(), 1, "should detect table evidence");
        assert!(evidence[0].contains("Bash"), "should include tool name");
        assert!(
            evidence[0].contains("成功率"),
            "should include table content"
        );
    }

    #[test]
    fn extract_evidence_detects_numeric_list() {
        let messages = vec![ConversationMessage::tool_result(
            "call_2",
            "Bash",
            "100\n120\n95\n88\n110",
            false,
        )];
        let evidence = super::extract_evidence_before_compaction(&messages);
        assert_eq!(evidence.len(), 1, "should detect numeric list evidence");
    }

    #[test]
    fn extract_evidence_detects_keywords() {
        let messages = vec![ConversationMessage::tool_result(
            "call_3",
            "Bash",
            "基准测试结果: UA=0.85, 协议=HTTP/2",
            false,
        )];
        let evidence = super::extract_evidence_before_compaction(&messages);
        assert_eq!(evidence.len(), 1, "should detect keyword evidence");
        assert!(evidence[0].contains("基准"));
    }

    #[test]
    fn extract_evidence_ignores_plain_text() {
        let messages = vec![ConversationMessage::tool_result(
            "call_4",
            "Read",
            "this is a regular file\nwith some text\nnothing special here",
            false,
        )];
        let evidence = super::extract_evidence_before_compaction(&messages);
        assert!(
            evidence.is_empty(),
            "should not detect evidence in plain text"
        );
    }

    #[test]
    fn extract_evidence_truncates_to_500_chars() {
        let long_table = format!(
            "| col1 | col2 |\n|------|------|\n{}",
            "| val | data |".repeat(100)
        );
        let messages = vec![ConversationMessage::tool_result(
            "call_5",
            "Bash",
            &long_table,
            false,
        )];
        let evidence = super::extract_evidence_before_compaction(&messages);
        assert_eq!(evidence.len(), 1);
        // 每条不超过 EVIDENCE_SUMMARY_MAX_CHARS(500)字符(含前缀和截断标记)
        assert!(
            evidence[0].chars().count() <= super::EVIDENCE_SUMMARY_MAX_CHARS,
            "evidence should be truncated to <= {} chars, got {}",
            super::EVIDENCE_SUMMARY_MAX_CHARS,
            evidence[0].chars().count()
        );
        assert!(
            evidence[0].ends_with('…'),
            "truncated evidence should end with …"
        );
    }

    #[test]
    fn extract_evidence_skips_non_tool_blocks() {
        let messages = vec![
            ConversationMessage::user_text(
                "| 方案 | 成功率 |\n|------|--------|\n| A    | 95%    |",
            ),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "对比矩阵: A=100 B=200".to_string(),
            }]),
        ];
        let evidence = super::extract_evidence_before_compaction(&messages);
        assert!(
            evidence.is_empty(),
            "should only extract from ToolResult blocks, not user/assistant text"
        );
    }

    #[test]
    fn extract_evidence_handles_multiple_tool_results() {
        let messages = vec![
            ConversationMessage::tool_result("call_1", "Bash", "基准: 100 req/s", false),
            ConversationMessage::tool_result("call_2", "Read", "plain file content", false),
            ConversationMessage::tool_result(
                "call_3",
                "Bash",
                "| 对比 | A | B |\n|------|---|---|\n| 结果 | 1 | 2 |",
                false,
            ),
        ];
        let evidence = super::extract_evidence_before_compaction(&messages);
        assert_eq!(
            evidence.len(),
            2,
            "should detect evidence in call_1 and call_3"
        );
    }
}
