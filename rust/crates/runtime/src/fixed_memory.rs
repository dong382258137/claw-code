//! 锚点型固定记忆快照(Fixed Memory)—— 结论行 + 证据锚点 + 置信来源 + 定点验证指引。
//!
//! # 背景
//!
//! 长链任务中,AI 的"当前目标、已完成项、历史教训"跨压缩后需要一块**零 LLM 成本**
//! 的稳定锚点:请求构造时读既有落盘数据(task_state.json + lessons.jsonl)规则式收敛
//! 成固定体积简报,注入后保持字节稳定(配合 DeepSeek 缓存 TTL),不随上下文波动。
//!
//! 与 task_state / lessons 的区别:
//! - task_state / lessons 是**数据源**(由 runtime 在 turn 结束时维护);
//! - 本模块是**消费侧快照**:把数据源收敛为单条 markdown 简报 + 内容指纹,
//!   按 TTL 判断是否需要重建,供请求构造注入(不调 LLM,只读落盘数据)。
//!
//! # 结构
//!
//! - `content`:渲染后的 markdown 简报(固定字符数上限 [`FIXED_MEMORY_MAX_CHARS`])。
//! - `fingerprint`:内容的 FNV-1a 64 稳定哈希(跨进程确定,用于缓存对齐/变更检测)。
//! - `injected_at_ms`:上次注入/重建时间戳,配合 [`FIXED_MEMORY_TTL_SECS`] 决定刷新时机。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 快照 TTL(秒):对齐 DeepSeek 缓存 TTL,TTL 内复用旧字节以命中 prompt 缓存。
pub const FIXED_MEMORY_TTL_SECS: i64 = 300;

/// 前瞻触发窗口(毫秒):TTL 的 90%。距上次请求 > 此窗口时,下一请求大概率
/// 冷启(期间无请求重置 TTL),把 LLM 更新 fixed_memory 的成本摊进重建轮。
pub const FIXED_MEMORY_PRECEDING_WINDOW_MS: i64 = 270_000;

/// LLM 摘要最小间隔(毫秒):防抖,避免每 ~5 分钟空转一次 LLM 调用。
pub const FIXED_MEMORY_MIN_SUMMARY_INTERVAL_MS: i64 = 60_000;

/// 固定记忆持久化文件名(位于 `.claw/` 下)。
pub const FIXED_MEMORY_FILE: &str = "fixed_memory.json";

/// 简报内容的最大字符数(恒定体积,避免无限增长)。
pub const FIXED_MEMORY_MAX_CHARS: usize = 1500;

/// 锚点型固定记忆快照:结论行 + 证据锚点 + 置信来源 + 定点验证指引。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixedMemorySnapshot {
    /// 渲染后的 markdown 简报正文。
    pub content: String,
    /// 内容的 FNV-1a 64 稳定指纹(跨进程确定)。
    pub fingerprint: u64,
    /// 快照注入/重建时刻(epoch ms),用于 TTL 判定。
    pub injected_at_ms: i64,
    /// 上次 LLM 摘要时会话消息数(摘要点游标)。0 = 未摘要/全量。
    /// 缺失(旧格式)时 serde 默认补 0,保证旧快照兼容加载。
    #[serde(default)]
    pub last_summary_msg_index: i64,
}

/// 返回固定记忆快照的落盘路径:`<root>/.claw/fixed_memory.json`。
#[must_use]
pub fn fixed_memory_path(root: &Path) -> PathBuf {
    root.join(".claw").join(FIXED_MEMORY_FILE)
}

/// 从 `<root>/.claw/fixed_memory.json` 加载快照,失败/不存在返回 None。
#[must_use]
pub fn load(root: &Path) -> Option<FixedMemorySnapshot> {
    let content = std::fs::read_to_string(fixed_memory_path(root)).ok()?;
    serde_json::from_str(&content).ok()
}

/// 持久化快照到磁盘(自动创建 `.claw/` 目录)。失败返回错误信息(不 panic)。
pub fn save(root: &Path, snap: &FixedMemorySnapshot) -> Result<(), String> {
    let path = fixed_memory_path(root);
    let content = serde_json::to_string_pretty(snap).map_err(|e| format!("serialize: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    std::fs::write(path, content).map_err(|e| format!("write: {e}"))
}

/// 计算内容的 FNV-1a 64 稳定哈希(自实现,无第三方依赖)。
///
/// 确定性要求:对同一字节序列,任意进程/平台计算结果一致(按 UTF-8 字节遍历,
/// 不做大小写/空白归一化,保证"字节即指纹")。空串哈希即 FNV offset basis。
#[must_use]
pub fn fingerprint(content: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// 判定快照是否需要刷新:无注入记录 → true;超过 TTL(严格大于)→ true;否则 false。
#[must_use]
pub fn should_refresh(injected_at_ms: Option<i64>, now_ms: i64) -> bool {
    match injected_at_ms {
        None => true,
        Some(t) => now_ms - t > FIXED_MEMORY_TTL_SECS * 1000,
    }
}

/// 固定记忆简报中的易变块标题(任务推进时高频变化,应从前缀剥离)。
const VOLATILE_HEADINGS: [&str; 2] = ["当前目标", "下一步"];
/// 稳定块标题:命中即切回稳定区。
const STABLE_HEADINGS: [&str; 3] = ["已完成项", "历史教训", "注"];

/// 将固定记忆简报拆分为"稳定段(注入 messages 前缀)"与"易变段(注入尾部
/// 冻结槽位块)"。
///
/// 动机(C1):简报的 `当前目标` / `下一步` 随任务推进高频变化;若整体注入
/// messages[0] 前缀,任何重建都会使其后全部历史消息缓存失效。拆出后:
/// 前缀只保留低频稳定内容(已完成项 / 历史教训 / 注脚),易变段移到尾部
/// 冻结槽位块 —— 变化只影响尾部消息,前缀字节稳定。
///
/// 兼容两种简报格式:
/// - LLM 简报:`当前目标：...` / `下一步：...` 独立块;
/// - 规则简报([`build_snapshot`]):`- 当前目标: ...` 单行 bullet。
///
/// 无易变块时返回 `(原内容, None)`。
#[must_use]
pub fn split_stable_volatile(content: &str) -> (String, Option<String>) {
    let mut stable = String::with_capacity(content.len());
    let mut volatile = String::new();
    // false = 稳定区;true = 易变区(当前目标/下一步)。
    let mut in_volatile = false;

    for line in content.lines() {
        let trimmed = line.trim_start();
        let bare = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .unwrap_or(trimmed);
        if is_heading(bare, &VOLATILE_HEADINGS) {
            in_volatile = true;
        } else if is_heading(bare, &STABLE_HEADINGS) {
            in_volatile = false;
        }
        if in_volatile {
            volatile.push_str(line);
            volatile.push('\n');
        } else {
            stable.push_str(line);
            stable.push('\n');
        }
    }

    let stable = stable.trim_end().to_string();
    let volatile = volatile.trim().to_string();
    let volatile = if volatile.is_empty() { None } else { Some(volatile) };
    (stable, volatile)
}

/// 判断 bare 行是否为给定标题之一(标题后跟中文或英文冒号)。
fn is_heading(bare: &str, headings: &[&str]) -> bool {
    headings.iter().any(|h| {
        bare.starts_with(h)
            && bare[h.len()..]
                .chars()
                .next()
                .is_some_and(|c| c == '：' || c == ':')
    })
}

/// 从 task_state + lessons 规则式收敛锚点型简报(零 LLM 调用,只读既有落盘数据)。
///
/// 空状态(task_state 无内容且 lessons 为空)返回 None。内容结构(markdown):
/// 标题行 + 当前目标 + 已完成项(findings → completed_subgoals → closed_tasks) +
/// 历史教训 + 注脚(置信来源 + 定点验证指引)。总字符数超
/// [`FIXED_MEMORY_MAX_CHARS`] 时按块从末尾截断(先丢 lessons、再丢已完成项尾部),
/// 保留标题与注脚。
#[must_use]
pub fn build_snapshot(root: &Path) -> Option<String> {
    let task_state = crate::task_state::TaskState::load(
        &root.join(".claw").join(crate::task_state::TASK_STATE_FILE),
    );
    let lessons = crate::lessons::load_recent_lessons(root, crate::lessons::LESSONS_INJECT_MAX);

    let goal = task_state
        .as_ref()
        .map(|ts| ts.goal.as_str())
        .unwrap_or_default();
    // 依次拼 findings → completed_subgoals → closed_tasks(均已按各自上限截断,
    // 与 task_state::render_for_prompt 数据来源一致)。
    let completed: Vec<String> = task_state
        .as_ref()
        .map(|ts| {
            ts.findings
                .iter()
                .chain(ts.completed_subgoals.iter())
                .chain(ts.closed_tasks.iter())
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    if goal.is_empty() && completed.is_empty() && lessons.is_empty() {
        return None;
    }

    // 分段组装,便于按块截断:goal → completed → lessons。
    let mut sections: Vec<String> = Vec::with_capacity(3);
    if !goal.is_empty() {
        sections.push(format!("- 当前目标: {goal}\n"));
    }
    if !completed.is_empty() {
        let mut block = String::from("- 已完成项:\n");
        for item in &completed {
            block.push_str(&format!("  · {item}\n"));
        }
        sections.push(block);
    }
    if !lessons.is_empty() {
        let mut block = String::from("- 历史教训:\n");
        for l in &lessons {
            block.push_str(&format!("  · {}\n", l.lesson));
        }
        sections.push(block);
    }

    let full = assemble(&sections);
    if full.chars().count() <= FIXED_MEMORY_MAX_CHARS {
        return Some(full);
    }

    // 第一步:先丢 lessons 块(位于末尾,整块移除)。
    let mut no_lessons = sections;
    if no_lessons
        .last()
        .is_some_and(|s| s.starts_with("- 历史教训:"))
    {
        no_lessons.pop();
    }
    let trimmed = assemble(&no_lessons);
    if trimmed.chars().count() <= FIXED_MEMORY_MAX_CHARS {
        return Some(trimmed);
    }

    // 第二步:再丢已完成项尾部(逐条从末尾删除,直到满足上限;删空则整段移除)。
    if let Some(idx) = no_lessons.iter().position(|s| s.starts_with("- 已完成项:")) {
        loop {
            let assembled = assemble(&no_lessons);
            if assembled.chars().count() <= FIXED_MEMORY_MAX_CHARS {
                return Some(assembled);
            }
            let block = &mut no_lessons[idx];
            match block.rfind("\n  · ") {
                Some(pos) => block.truncate(pos + 1),
                None => {
                    // 仅剩单条或空块:整个已完成项段移除。
                    no_lessons.remove(idx);
                    break;
                }
            }
        }
    }
    Some(assemble(&no_lessons))
}

/// 组装完整简报:标题 + 分段 + 注脚(置信来源 + 定点验证指引)。
fn assemble(sections: &[String]) -> String {
    let mut out = String::from("# 固定记忆(任务简报 · 锚点型)\n");
    for s in sections {
        out.push_str(s);
    }
    out.push_str("- 注:以上由 runtime 自动记录,与当前对话不符时以最新对话为准。\n");
    out.push_str("- 验证指引:对已完成项细节不确定时,先 read 锚点列出的文件/位置验证;锚点不足时用一次 git status/diff 精确确认,不要全库搜索。\n");
    out
}

/// 请求构造用的注入决策:决定本轮应注入哪份快照(或 None)。
///
/// 决策顺序:
/// - `prev` 存在 且 (`cache_hot` 或 距上次注入 ≤ TTL) → 复用 prev(字节不变)。
///   其中 `cache_hot` 表示上一轮请求命中缓存前缀(`cache_read > 0`),缓存仍
///   活跃 —— 即使已超固定 300s 计时也复用,避免主动打断本可命中的前缀
///   (A 修复:固定计时与实际缓存活跃窗口脱钩)。
/// - `built` 非空 → 新建快照(content=built, fingerprint 重算, injected_at_ms=now)。
/// - 否则 `prev` 存在 → 复用 prev(内容为空时保持旧字节)。
/// - 否则 → None(无旧快照且无新内容,不注入)。
#[must_use]
pub fn next_injection(
    prev: Option<&FixedMemorySnapshot>,
    built: Option<String>,
    now_ms: i64,
    cache_hot: bool,
) -> Option<FixedMemorySnapshot> {
    if let Some(p) = prev {
        if cache_hot || !should_refresh(Some(p.injected_at_ms), now_ms) {
            return Some(p.clone());
        }
    }
    if let Some(built) = built {
        if !built.trim().is_empty() {
            let fp = fingerprint(&built);
            return Some(FixedMemorySnapshot {
                content: built,
                fingerprint: fp,
                injected_at_ms: now_ms,
                last_summary_msg_index: 0,
            });
        }
    }
    // 内容为空时保持旧字节(prev 存在则复用,否则 None)。
    prev.cloned()
}

/// 复用路径字节漂移检测:热窗内(时间戳一致)内容必须逐字节一致,
/// 不一致即前缀命中线回退信号(重建会更新 injected_at_ms,不属漂移)。
#[must_use]
pub fn has_byte_drift(prev: &FixedMemorySnapshot, next: &FixedMemorySnapshot) -> bool {
    prev.injected_at_ms == next.injected_at_ms && prev.content != next.content
}

/// 把「上次摘要点之后」的增量消息渲染成 LLM 摘要 prompt(fixed_memory 前瞻触发)。
///
/// 每条消息一行:`<序号>. <role>: <块摘要>`,块摘要含文本/thinking/tool use/
/// tool result(输出截断到 [`LLM_SUMMARY_BLOCK_MAX_CHARS`] 控制 prompt 体积)。
/// 末尾固定指令约束输出结构(当前目标 / 已完成项含文件锚点 / 历史教训 /
/// 下一步)与幻觉护栏(只总结已发生内容,不推断新增)。
pub fn build_llm_summary_prompt(
    messages_since_marker: &[crate::session::ConversationMessage],
) -> String {
    /// 单个块摘要的最大字符数(截断保护,防 tool result 撑爆 prompt)。
    const LLM_SUMMARY_BLOCK_MAX_CHARS: usize = 500;

    fn summarize_block(block: &crate::session::ContentBlock) -> String {
        let raw = match block {
            crate::session::ContentBlock::Text { text } => text.clone(),
            crate::session::ContentBlock::Thinking { thinking, .. } => {
                format!("thinking: {thinking}")
            }
            crate::session::ContentBlock::ToolUse { name, input, .. } => {
                format!("tool_use {name}({input})")
            }
            crate::session::ContentBlock::ToolResult {
                tool_name,
                output,
                is_error,
                ..
            } => format!(
                "tool_result {tool_name}: {}{output}",
                if *is_error { "error " } else { "" }
            ),
        };
        if raw.chars().count() <= LLM_SUMMARY_BLOCK_MAX_CHARS {
            raw
        } else {
            let mut truncated: String = raw.chars().take(LLM_SUMMARY_BLOCK_MAX_CHARS).collect();
            truncated.push('…');
            truncated
        }
    }

    let mut transcript = Vec::new();
    for (idx, message) in messages_since_marker.iter().enumerate() {
        let role = match message.role {
            crate::session::MessageRole::System => "system",
            crate::session::MessageRole::User => "user",
            crate::session::MessageRole::Assistant => "assistant",
            crate::session::MessageRole::Tool => "tool",
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
        "以下是固定记忆上次摘要点之后新增的对话消息(role: user / assistant / tool):\n\
         \n\
         {}\n\
         \n\
         请输出固定记忆简报,结构为:当前目标、已完成项(每项一行,含涉及的文件路径/位置锚点)、\
         历史教训、下一步。只总结以上对话中已发生的内容,不要推断或新增。用中文。",
        transcript.join("\n")
    )
}

/// LLM 摘要变更门控 + 调用入口:增量消息为空或全为空文本/tool_result 时返回
/// None(跳过 LLM 调用,避免每 ~5 分钟空转);否则走全局压缩摘要 client
/// (未注册 / 失败 / 空输出 → None,静默降级,不打印告警)。
#[must_use]
pub fn maybe_llm_summary(
    root: &std::path::Path,
    messages_since_marker: &[crate::session::ConversationMessage],
) -> Option<String> {
    // 变更门控:存在任何非空文本块或 tool_result 块才算"有实质新内容"。
    let has_substantive = messages_since_marker.iter().any(|m| {
        m.blocks.iter().any(|b| match b {
            crate::session::ContentBlock::Text { text } => !text.trim().is_empty(),
            crate::session::ContentBlock::ToolResult { output, .. } => !output.trim().is_empty(),
            _ => false,
        })
    });
    if !has_substantive {
        return None;
    }
    // root 预留:幻觉交叉校验(P1)已在 conversation.rs 落盘前用 root 读取
    // task_state 核对(见 cross_validate_with_task_state),此处仅保持签名一致。
    let _ = root;
    crate::compact::llm_summarize(&build_llm_summary_prompt(messages_since_marker))
}

/// 幻觉交叉校验护栏(P1):用规则通道 `task_state.findings` 交叉校验 LLM 生成的
/// 固定记忆简报,防止 LLM 编造未发生事项(LLM 声称已发生的 vs 规则实际提取的)。
///
/// 逻辑:
/// - 加载 `<root>/.claw/task_state.json`,无 task_state(或 findings 为空)则
///   原样返回 `llm_brief`(无规则证据可对照,不标注)。
/// - 对每条 finding,取「截断到 20 字符的子串」作关键词(避免整句匹配失败),
///   若 `llm_brief`(小写)不含该关键词(小写)→ 记为「规则确认但简报未体现」。
/// - 存在未体现项时在 `llm_brief` 末尾追加一行注脚:
///   `\n- ⚠ 规则通道确认但简报未体现:{f1}; {f2}`(每条截断 40 字符);
///   超过 3 条只列前 3 并加"等 N 条"。
/// - 总字符数超 [`FIXED_MEMORY_MAX_CHARS`] 时截断注脚优先(保留简报主体,
///   注脚不够截则整体丢弃,不动简报正文)。
#[must_use]
pub fn cross_validate_with_task_state(llm_brief: &str, root: &Path) -> String {
    /// finding 关键词长度:取截断到 20 字符的子串作匹配关键词。
    const KEYWORD_CHARS: usize = 20;
    /// 注脚中单条 finding 的最大字符数。
    const FOOTER_ITEM_CHARS: usize = 40;
    /// 注脚最多列出的未体现项条数,超出加"等 N 条"。
    const FOOTER_MAX_ITEMS: usize = 3;

    let task_state = crate::task_state::TaskState::load(
        &root.join(".claw").join(crate::task_state::TASK_STATE_FILE),
    );
    let Some(ts) = task_state else {
        return llm_brief.to_string();
    };
    if ts.findings.is_empty() {
        return llm_brief.to_string();
    }

    let brief_lower = llm_brief.to_lowercase();
    let missing: Vec<&str> = ts
        .findings
        .iter()
        .filter(|f| {
            let keyword: String = f.chars().take(KEYWORD_CHARS).collect();
            !brief_lower.contains(&keyword.to_lowercase())
        })
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return llm_brief.to_string();
    }

    let total = missing.len();
    let shown: Vec<String> = missing
        .iter()
        .take(FOOTER_MAX_ITEMS)
        .map(|f| f.chars().take(FOOTER_ITEM_CHARS).collect())
        .collect();
    let mut footer = format!("\n- ⚠ 规则通道确认但简报未体现:{}", shown.join("; "));
    if total > FOOTER_MAX_ITEMS {
        footer.push_str(&format!(" 等 {total} 条"));
    }

    // 超 FIXED_MEMORY_MAX_CHARS 时截断注脚优先:从注脚尾部截掉超额字符,
    // 截完还不够(注脚比超额短)则整体丢弃注脚,简报主体保持原样。
    let over = llm_brief.chars().count() as i64 + footer.chars().count() as i64
        - FIXED_MEMORY_MAX_CHARS as i64;
    if over > 0 {
        let keep = (footer.chars().count() as i64 - over).max(0) as usize;
        footer = footer.chars().take(keep).collect();
    }

    let mut out = llm_brief.to_string();
    if !footer.trim().is_empty() {
        out.push_str(&footer);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!(
            "claw-fixed-memory-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        (tmp.clone(), tmp)
    }

    #[test]
    fn split_stable_volatile_partitions_llm_brief() {
        let content = "当前目标：为 3买/3卖 信号新增过滤层\n\n\
                       已完成项：\n\
                       - 已读 types.py 确认数据结构\n\n\
                       历史教训：\n\
                       - 对话中无显式历史教训记录。\n\n\
                       下一步：\n\
                       - 继续补齐调查空白\n\
                       - ⚠ 规则通道确认但简报未体现:结论：**允许**。";
        let (stable, volatile) = split_stable_volatile(content);
        let v = volatile.expect("volatile present");
        assert!(v.contains("当前目标") && v.contains("下一步"));
        assert!(v.contains("继续补齐调查空白"));
        assert!(!v.contains("已完成项"));
        assert!(stable.contains("已完成项") && stable.contains("历史教训"));
        assert!(!stable.contains("当前目标"));
        assert!(!stable.contains("下一步"));
    }

    #[test]
    fn split_stable_volatile_partitions_rule_brief() {
        let content = "# 固定记忆(任务简报 · 锚点型)\n\
                       - 当前目标: 修复模块 A\n\
                       - 已完成项:\n\
                         · 已修复登录 401\n\
                       - 历史教训:\n\
                         · git stash 用相对路径\n\
                       - 注:以上由 runtime 自动记录。\n\
                       - 验证指引:先 read 锚点文件。";
        let (stable, volatile) = split_stable_volatile(content);
        let v = volatile.expect("volatile present");
        assert!(v.contains("当前目标"));
        assert!(!v.contains("已完成项"));
        assert!(stable.contains("已完成项"));
        assert!(stable.contains("历史教训"));
        assert!(stable.contains("注:"));
    }

    #[test]
    fn split_stable_volatile_no_volatile_returns_none() {
        let content = "# 固定记忆\n- 已完成项:\n  · aaa\n- 历史教训:\n  · bbb";
        let (stable, volatile) = split_stable_volatile(content);
        assert_eq!(volatile, None);
        assert_eq!(stable, content);
    }

    fn write_sample_state(root: &Path) {
        let ts = crate::task_state::TaskState {
            goal: "重构 auth 模块,兼容旧 Session 格式".to_string(),
            findings: vec![
                "关键结论:拆分 token 校验".to_string(),
                "根因:缓存失效".to_string(),
            ],
            completed_subgoals: vec!["完成迁移测试".to_string()],
            closed_tasks: vec!["登录 401 修复: 6/6 PASS".to_string()],
            ..Default::default()
        };
        ts.save(&root.join(".claw").join(crate::task_state::TASK_STATE_FILE))
            .expect("save task_state");

        crate::lessons::append_lessons(
            root,
            &["git stash 需用相对 cwd 的路径,否则误弹旧 stash".to_string()],
        )
        .expect("append lessons");
    }

    #[test]
    fn build_snapshot_includes_goal_items_lessons_and_footer() {
        let (tmp, root) = temp_root("build");
        write_sample_state(&root);

        let built = build_snapshot(&root).expect("should build");
        assert!(built.contains("固定记忆"), "header: {built}");
        assert!(built.contains("当前目标"), "goal line: {built}");
        assert!(built.contains("重构 auth 模块"), "goal content: {built}");
        assert!(built.contains("已完成项"), "items section: {built}");
        assert!(built.contains("根因:缓存失效"), "finding: {built}");
        assert!(built.contains("完成迁移测试"), "subgoal: {built}");
        assert!(built.contains("登录 401 修复"), "closed task: {built}");
        assert!(built.contains("历史教训"), "lessons section: {built}");
        assert!(built.contains("git stash 需用相对"), "lesson: {built}");
        assert!(built.contains("验证指引"), "footer: {built}");
        assert!(built.contains("不要全库搜索"), "footer: {built}");
        assert!(built.contains("read"), "footer read guidance: {built}");
        assert!(
            built.contains("与当前对话不符时以最新对话为准"),
            "footer: {built}"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn build_snapshot_empty_state_returns_none() {
        let (tmp, root) = temp_root("empty");
        // 无任何落盘数据
        assert!(build_snapshot(&root).is_none());

        // 落盘了但 task_state 为空且无 lessons
        crate::task_state::TaskState::default()
            .save(&root.join(".claw").join(crate::task_state::TASK_STATE_FILE))
            .expect("save empty task_state");
        assert!(build_snapshot(&root).is_none());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn build_snapshot_truncates_from_tail_keeping_header_and_footer() {
        let (tmp, root) = temp_root("trunc");
        // 构造接近各自上限的大体积数据,强制触发截断路径
        let ts = crate::task_state::TaskState {
            goal: "目".repeat(150),
            findings: (0..6).map(|_| "关".repeat(120)).collect(),
            completed_subgoals: (0..6).map(|_| "步".repeat(120)).collect(),
            closed_tasks: (0..6).map(|_| "收".repeat(120)).collect(),
            ..Default::default()
        };
        ts.save(&root.join(".claw").join(crate::task_state::TASK_STATE_FILE))
            .expect("save task_state");
        crate::lessons::append_lessons(
            &root,
            &(0..5)
                .map(|i| format!("{i}{}", "教".repeat(200)))
                .collect::<Vec<_>>(),
        )
        .expect("append lessons");

        let built = build_snapshot(&root).expect("should build");
        assert!(
            built.chars().count() <= FIXED_MEMORY_MAX_CHARS,
            "len={}",
            built.chars().count()
        );
        assert!(built.starts_with("# 固定记忆"), "header preserved: {built}");
        assert!(built.contains("验证指引"), "footer preserved: {built}");
        assert!(built.contains("不要全库搜索"), "footer preserved: {built}");
        assert!(built.contains("- 当前目标: "), "goal kept: {built}");
        // 先丢 lessons → 截断后不应再有历史教训段
        assert!(
            !built.contains("历史教训"),
            "lessons dropped first: {built}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn should_refresh_ttl_boundaries() {
        let now = 1_000_000_000i64;
        // None → true
        assert!(should_refresh(None, now));
        // 刚好等于 TTL → false
        assert!(!should_refresh(
            Some(now - FIXED_MEMORY_TTL_SECS * 1000),
            now
        ));
        // 略小于 TTL → false
        assert!(!should_refresh(
            Some(now - FIXED_MEMORY_TTL_SECS * 1000 + 1),
            now
        ));
        // 严格大于 TTL → true
        assert!(should_refresh(
            Some(now - FIXED_MEMORY_TTL_SECS * 1000 - 1),
            now
        ));
    }

    #[test]
    fn save_and_load_roundtrip_fields_match() {
        let (tmp, root) = temp_root("roundtrip");
        let snap = FixedMemorySnapshot {
            content: "# 固定记忆(任务简报 · 锚点型)\n- 当前目标: 测试\n".to_string(),
            fingerprint: fingerprint("样本内容"),
            injected_at_ms: 123_456,
            last_summary_msg_index: 7,
        };
        save(&root, &snap).expect("save");
        let loaded = load(&root).expect("load");
        assert_eq!(loaded, snap);
        assert_eq!(loaded.content, snap.content);
        assert_eq!(loaded.fingerprint, snap.fingerprint);
        assert_eq!(loaded.injected_at_ms, snap.injected_at_ms);
        assert_eq!(
            loaded.last_summary_msg_index, snap.last_summary_msg_index,
            "roundtrip must preserve last_summary_msg_index"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_legacy_snapshot_without_summary_index_defaults_to_zero() {
        // 旧格式 JSON(无 last_summary_msg_index 字段)必须兼容加载,默认 0。
        let (tmp, root) = temp_root("legacy");
        std::fs::create_dir_all(root.join(".claw")).expect("mkdir .claw");
        std::fs::write(
            root.join(".claw").join(FIXED_MEMORY_FILE),
            r#"{"content":"旧格式简报","fingerprint":123,"injected_at_ms":456}"#,
        )
        .expect("write legacy snapshot");
        let loaded = load(&root).expect("load legacy");
        assert_eq!(loaded.content, "旧格式简报");
        assert_eq!(loaded.last_summary_msg_index, 0, "缺失字段默认 0(全量)");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_missing_or_corrupt_returns_none() {
        let (tmp, root) = temp_root("load-none");
        assert!(load(&root).is_none());
        // 损坏 JSON
        std::fs::create_dir_all(root.join(".claw")).expect("mkdir .claw");
        std::fs::write(root.join(".claw").join(FIXED_MEMORY_FILE), "{not json")
            .expect("write corrupt");
        assert!(load(&root).is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn fingerprint_is_deterministic_and_content_sensitive() {
        // 同一内容两次调用结果相同(跨调用确定性)
        assert_eq!(
            fingerprint("锚点内容 abc123"),
            fingerprint("锚点内容 abc123")
        );
        // 空串哈希即 FNV offset basis(算法绝对锚点)
        assert_eq!(fingerprint(""), 0xcbf2_9ce4_8422_2325);
        // 不同内容结果不同
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
        assert_ne!(fingerprint("结论 A"), fingerprint("结论 B"));
    }

    #[test]
    fn next_injection_fresh_reuses_prev_bytes() {
        let now = 1_000_000_000i64;
        let prev = FixedMemorySnapshot {
            content: "旧简报字节".to_string(),
            fingerprint: fingerprint("旧简报字节"),
            injected_at_ms: now - 1_000, // 未超 TTL
            last_summary_msg_index: 0,
        };
        let got =
            next_injection(Some(&prev), Some("新内容".to_string()), now, false).expect("some");
        assert_eq!(got.content, prev.content, "fresh 应复用旧字节");
        assert_eq!(got.fingerprint, prev.fingerprint);
        assert_eq!(got.injected_at_ms, prev.injected_at_ms);
    }

    #[test]
    fn next_injection_stale_with_build_creates_new() {
        let now = 1_000_000_000i64;
        let prev = FixedMemorySnapshot {
            content: "旧简报字节".to_string(),
            fingerprint: fingerprint("旧简报字节"),
            injected_at_ms: now - FIXED_MEMORY_TTL_SECS * 1000 - 1, // 已超 TTL
            last_summary_msg_index: 0,
        };
        let built = "新构建的简报".to_string();
        let got = next_injection(Some(&prev), Some(built.clone()), now, false).expect("some");
        assert_eq!(got.content, built, "stale 应重建内容");
        assert_eq!(got.fingerprint, fingerprint(&built));
        assert_eq!(got.injected_at_ms, now, "注入时间应更新");
        assert_ne!(got.content, prev.content);
    }

    #[test]
    fn next_injection_stale_with_empty_build_reuses_prev() {
        let now = 1_000_000_000i64;
        let prev = FixedMemorySnapshot {
            content: "旧简报字节".to_string(),
            fingerprint: fingerprint("旧简报字节"),
            injected_at_ms: now - FIXED_MEMORY_TTL_SECS * 1000 - 1, // 已超 TTL
            last_summary_msg_index: 0,
        };
        // built = None
        let got = next_injection(Some(&prev), None, now, false).expect("some");
        assert_eq!(got.content, prev.content, "空 build 应保持旧字节");
        assert_eq!(got.injected_at_ms, prev.injected_at_ms);
        // built = Some(空白串)
        let got2 = next_injection(Some(&prev), Some("   ".to_string()), now, false).expect("some");
        assert_eq!(got2.content, prev.content);
    }

    #[test]
    fn next_injection_no_prev_empty_build_returns_none() {
        let now = 1_000_000_000i64;
        assert!(next_injection(None, None, now, false).is_none());
        assert!(next_injection(None, Some("   ".to_string()), now, false).is_none());
    }

    #[test]
    fn next_injection_cache_hot_reuses_stale_prev() {
        // A 修复回归:缓存仍热(cache_read>0)时,即使已超固定 TTL 也复用旧字节,
        // 不主动打断本可命中的前缀(固定 300s 计时与实际缓存活跃窗口脱钩)。
        let now = 1_000_000_000i64;
        let prev = snap("旧简报字节", now - FIXED_MEMORY_TTL_SECS * 1000 - 1); // 已超 TTL
        let got = next_injection(Some(&prev), Some("新内容".to_string()), now, true).expect("some");
        assert_eq!(
            got.content, prev.content,
            "cache_hot 应复用旧字节,即使已超 TTL"
        );
        assert_eq!(
            got.injected_at_ms, prev.injected_at_ms,
            "复用不得刷新注入时间戳"
        );
        assert_eq!(got.fingerprint, prev.fingerprint);
    }

    #[test]
    fn next_injection_cache_cold_still_rebuilds_stale() {
        // 对照:缓存已冷(cache_read=0)且超 TTL → 走重建分支
        let now = 1_000_000_000i64;
        let prev = snap("旧简报字节", now - FIXED_MEMORY_TTL_SECS * 1000 - 1);
        let built = "新构建的简报".to_string();
        let got = next_injection(Some(&prev), Some(built.clone()), now, false).expect("some");
        assert_eq!(got.content, built, "cache_cold + stale 应重建");
        assert_eq!(got.injected_at_ms, now);
    }

    fn snap(content: &str, injected_at_ms: i64) -> FixedMemorySnapshot {
        FixedMemorySnapshot {
            content: content.to_string(),
            fingerprint: fingerprint(content),
            injected_at_ms,
            last_summary_msg_index: 0,
        }
    }

    #[test]
    fn has_byte_drift_same_ts_different_content_is_true() {
        // 热窗复用应逐字节一致,漂移即前缀命中线回退信号
        assert!(has_byte_drift(&snap("旧字节", 1000), &snap("新字节", 1000)));
    }

    #[test]
    fn has_byte_drift_same_ts_same_content_is_false() {
        assert!(!has_byte_drift(
            &snap("相同字节", 1000),
            &snap("相同字节", 1000)
        ));
    }

    #[test]
    fn has_byte_drift_different_ts_is_false() {
        // 异时间戳 = 重建属预期,不报警
        assert!(!has_byte_drift(
            &snap("旧字节", 1000),
            &snap("新字节", 2000)
        ));
    }

    // ---- fixed_memory LLM 写入(P0):prompt 构建 / 变更门控 / 全局 client 路径 ----

    #[test]
    fn build_llm_summary_prompt_includes_transcript_and_instructions() {
        use crate::session::{ContentBlock, ConversationMessage, MessageRole};
        let messages = vec![
            ConversationMessage::user_text("修复登录 401"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "根因:缓存失效,已在 auth.rs 拆分 token 校验".to_string(),
            }]),
            ConversationMessage::tool_result("1", "Edit", "auth.rs 修改完成", false),
        ];
        let prompt = build_llm_summary_prompt(&messages);
        // 增量消息文本必须进入 prompt(供 LLM 摘要输入)
        assert!(prompt.contains("修复登录 401"), "user text: {prompt}");
        assert!(
            prompt.contains("auth.rs"),
            "assistant 文本/文件锚点应保留: {prompt}"
        );
        assert!(
            prompt.contains("tool_result Edit"),
            "tool result 摘要应保留: {prompt}"
        );
        // 固定结构指令(「已完成项」+「文件路径/位置锚点」)必须在末尾
        assert!(
            prompt.contains("请输出固定记忆简报"),
            "fixed instruction: {prompt}"
        );
        assert!(prompt.contains("已完成项"), "structure: {prompt}");
        assert!(
            prompt.contains("文件路径/位置锚点"),
            "anchor requirement: {prompt}"
        );
        assert!(
            prompt.contains("不要推断或新增"),
            "hallucination guard: {prompt}"
        );
        // 空增量 → prompt 仍含指令但不含消息行
        let empty_prompt = build_llm_summary_prompt(&[]);
        assert!(
            empty_prompt.contains("请输出固定记忆简报"),
            "empty transcript still carries instructions: {empty_prompt}"
        );
        assert!(
            !empty_prompt.contains("0. "),
            "no message lines: {empty_prompt}"
        );
    }

    #[test]
    fn maybe_llm_summary_empty_or_blank_incremental_returns_none() {
        use crate::session::{ContentBlock, ConversationMessage};
        let (tmp, root) = temp_root("gate");
        // 空增量 → None(不调用 LLM)
        assert!(maybe_llm_summary(&root, &[]).is_none());
        // 全为空文本 / 空 tool_result → None
        let blank = vec![
            ConversationMessage::user_text("   "),
            ConversationMessage::tool_result("1", "Bash", "", false),
        ];
        assert!(maybe_llm_summary(&root, &blank).is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// 路由型 fake client:仅对固定记忆摘要 prompt(含「固定记忆简报」标记)返回
    /// 固定文本;其它 prompt(如压缩摘要)返回 Err → 走启发式兜底,避免污染
    /// 依赖"全局未注册 → 启发式"的既有 compact 测试。
    struct FakeFixedMemorySummarizer;
    impl crate::compact::CompactionSummarizerClient for FakeFixedMemorySummarizer {
        fn summarize(&self, prompt: &str) -> Result<String, String> {
            if prompt.contains("固定记忆简报") {
                Ok(
                    "FAKE_LLM_BRIEF: 已完成登录 401 修复(路径 auth.rs),下一步补回归测试"
                        .to_string(),
                )
            } else {
                Err("not a fixed-memory prompt".to_string())
            }
        }
    }

    #[test]
    fn maybe_llm_summary_with_registered_client_returns_summary() {
        use crate::session::{ContentBlock, ConversationMessage};
        let (tmp, root) = temp_root("llm");
        // 注入全局摘要 client(OnceLock 单例:若此前已有注册则忽略,此时直接
        // 断言其对固定记忆 prompt 的行为 —— 路由型 fake 恒返回固定文本)。
        crate::compact::set_global_compaction_summarizer_client(std::sync::Arc::new(
            FakeFixedMemorySummarizer,
        ));
        let messages = vec![
            ConversationMessage::user_text("修复登录 401"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "已定位根因并在 auth.rs 完成修改".to_string(),
            }]),
        ];
        let got = maybe_llm_summary(&root, &messages).expect("LLM summary present");
        assert!(
            got.contains("FAKE_LLM_BRIEF"),
            "LLM 路径应返回 fake 固定文本: {got}"
        );
        assert!(got.contains("auth.rs"), "fake 摘要应含文件锚点: {got}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---- P1 幻觉交叉校验:规则通道 findings vs LLM 简报 ----

    fn write_findings(root: &Path, findings: Vec<String>) {
        let ts = crate::task_state::TaskState {
            goal: "重构 auth 模块".to_string(),
            findings,
            ..Default::default()
        };
        ts.save(&root.join(".claw").join(crate::task_state::TASK_STATE_FILE))
            .expect("save task_state");
    }

    #[test]
    fn cross_validate_no_task_state_returns_brief_unchanged() {
        let (tmp, root) = temp_root("xval-none");
        // 无 .claw/task_state.json → 无规则证据可对照,原样返回
        let brief = "# 固定记忆\n- 已完成项:\n  · 修复登录 401(auth.rs)";
        assert_eq!(cross_validate_with_task_state(brief, &root), brief);

        // findings 为空同样原样返回
        write_findings(&root, vec![]);
        assert_eq!(cross_validate_with_task_state(brief, &root), brief);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cross_validate_findings_all_covered_no_footer() {
        let (tmp, root) = temp_root("xval-all");
        write_findings(
            &root,
            vec![
                "关键结论:拆分 token 校验".to_string(),
                "根因:缓存失效".to_string(),
            ],
        );
        // LLM 简报已体现全部 findings(关键词取 finding 前 20 字符,简报须含
        // 完整前缀才算已体现)
        let brief =
            "# 固定记忆\n- 已完成项:\n  · 关键结论:拆分 token 校验\n  · 根因:缓存失效 已定位";
        let got = cross_validate_with_task_state(brief, &root);
        assert_eq!(got, brief, "findings 全覆盖 → 不应追加注脚: {got}");
        assert!(!got.contains("规则通道确认"), "{got}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cross_validate_partial_missing_appends_footer() {
        let (tmp, root) = temp_root("xval-partial");
        write_findings(
            &root,
            vec![
                "关键结论:拆分 token 校验".to_string(),
                "根因:缓存失效".to_string(),
                "修复:登录 401 已确认".to_string(),
            ],
        );
        // finding1 前 20 字符关键词完整出现在简报 → 已体现;finding2/3 未体现
        let brief = "# 固定记忆\n- 已完成项:\n  · 关键结论:拆分 token 校验";
        let got = cross_validate_with_task_state(brief, &root);
        // 简报主体保留 + 注脚追加,只列未体现项
        assert!(got.starts_with(brief), "简报主体应保留在前: {got}");
        assert!(got.contains("规则通道确认但简报未体现"), "注脚标记: {got}");
        assert!(got.contains("根因:缓存失效"), "未体现项1: {got}");
        assert!(got.contains("登录 401 已确认"), "未体现项2: {got}");
        // 已体现的 finding 不应进注脚:全文只出现 1 次(简报主体),注脚无
        assert_eq!(
            got.matches("拆分 token 校验").count(),
            1,
            "已体现项不得标注: {got}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cross_validate_many_missing_shows_first_three_with_count() {
        let (tmp, root) = temp_root("xval-many");
        let findings = (0..5).map(|i| format!("关键结论:{i} 号修复")).collect();
        write_findings(&root, findings);
        let brief = "# 固定记忆\n- 已完成项:\n  · 无关内容";
        let got = cross_validate_with_task_state(brief, &root);
        assert!(got.contains("等 5 条"), "超 3 条应带计数后缀: {got}");
        assert!(got.contains("0 号修复"), "前3条: {got}");
        assert!(got.contains("1 号修复"), "前3条: {got}");
        assert!(got.contains("2 号修复"), "前3条: {got}");
        assert!(!got.contains("3 号修复"), "第4条不应列出: {got}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cross_validate_oversize_truncates_footer_keeps_body() {
        let (tmp, root) = temp_root("xval-len");
        write_findings(
            &root,
            vec![format!(
                "关键结论:{}",
                "长".repeat(crate::task_state::TASK_FINDING_MAX_CHARS)
            )],
        );
        // 简报主体贴近上限:主体 1470 字符 + 注脚 ~58 字符必然超上限 → 注脚被截
        let brief = format!("# 固定记忆\n- 已完成项:\n  · {}", "主".repeat(1451));
        assert!(brief.chars().count() <= FIXED_MEMORY_MAX_CHARS);
        let got = cross_validate_with_task_state(&brief, &root);
        assert!(
            got.chars().count() <= FIXED_MEMORY_MAX_CHARS,
            "len={}",
            got.chars().count()
        );
        assert!(got.starts_with(&brief), "截断只应发生在注脚,简报主体保留");
        assert!(
            got.contains("规则通道确认但简报未体现"),
            "注脚应保留足够头部(超额 < 注脚总长): {got}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn cross_validate_finding_requires_20_char_keyword_match() {
        let (tmp, root) = temp_root("xval-keyword");
        // finding 截断到 20 字符作关键词:简报中仅出现 finding 尾部片段时,
        // 前 20 字符片段不在 → 仍判未体现(防整句误判为已体现)
        let finding = "关键结论:token 校验拆分到独立模块,已单测通过".to_string();
        write_findings(&root, vec![finding.clone()]);
        let brief = format!(
            "# 固定记忆\n- 已完成项:\n  · {}",
            // 取 finding 尾部(第 25 字符起),不含前 20 字符关键词
            finding.chars().skip(25).collect::<String>()
        );
        let got = cross_validate_with_task_state(&brief, &root);
        assert!(
            got.contains("规则通道确认但简报未体现"),
            "缺少前 20 字符关键词应判未体现: {got}"
        );
        // 对照:简报含完整 finding 前 20 字符 → 判已体现,无注脚
        let brief_full = format!("# 固定记忆\n- 已完成项:\n  · {finding}");
        let got_full = cross_validate_with_task_state(&brief_full, &root);
        assert!(
            !got_full.contains("规则通道确认"),
            "含关键词应已体现: {got_full}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
