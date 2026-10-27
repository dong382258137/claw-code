//! 收敛契约 · 打回率埋点(基线阶段,2026-09-05)。
//!
//! 口径对齐 docs/2026-09-05-convergence-contract-design.md §6.1.3:
//! 判定"某条 user 输入是否是对上一条 assistant 收尾答复的『打回』"——即用户因
//! 结果质量不满重新介入(质疑/纠错/要求重做/未解决/无输出)。用三层词表 + 两类
//! 剔除,避免把"同域新问题报告 / 用户自澄清"误计为打回。
//!
//! 本模块当前只做**埋点统计**(落盘会话级 `<session_id>-punchback.jsonl`,供与
//! 无契约状态基线对照),不介入请求构造 / 收敛门控(那是收敛契约 P1 阶段)。
//! 非关键路径:落盘失败返回 Err,调用方静默忽略,不阻断主流程。

use serde::Serialize;
use std::path::{Path, PathBuf};

/// 埋点文件名前缀:sessions 目录下 `<session_id>-punchback.jsonl`。
const PUNCHBACK_FILE_SUFFIX: &str = "-punchback.jsonl";

/// 会话级埋点文件路径(与 task_state 同构,见 `task_state::session_task_state_path`)。
#[must_use]
pub fn session_punchback_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}{PUNCHBACK_FILE_SUFFIX}"))
}

/// 打回类型(统计口径)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PunchbackClass {
    /// 强打回(窄口径):直接质疑上次结果 / 要求重做 / 未解决 / 无输出。
    Strong,
    /// 弱打回(宽口径):含双义词(有问题/缺/漏…),可能是同域新问题报告。
    Weak,
    /// 疑似推进:未命中打回词,但含"再/继续修"类,单独成桶不计入打回。
    Force,
    /// 非打回:ACK / 短输入 / 剔除前缀 / 无命中。
    None,
}

/// 纯承接词(精确匹配整条输入 → 不视为打回)。
const ACK_SET: &[&str] = &[
    "好",
    "好的",
    "好呀",
    "好嘞",
    "好勒",
    "ok",
    "okay",
    "嗯",
    "嗯嗯",
    "收到",
    "知道了",
    "明白了",
    "明白",
    "可以",
    "行",
    "行吧",
    "好吧",
    "好滴",
    "没问题",
    "了解",
    "了解了",
    "晓得了",
    "是",
    "对",
    "不错",
    "继续",
    "继续吧",
    "继续做",
    "继续干",
    "随便",
    "随意",
    "都行",
    "都行吧",
    "无所谓",
    "谢谢",
    "感谢",
];

/// 强打回词(窄口径):直接质疑上次收尾结果 / 重做 / 未解决 / 无输出。
const STRONG_PUNCH_WORDS: &[&str] = &[
    "不对",
    "不是",
    "错了",
    "怎么还",
    "怎么还是",
    "仍然",
    "依然",
    "依旧",
    "还是不行",
    "没解决",
    "还没",
    "未完成",
    "不成立",
    "无效",
    "没效果",
    "不起作用",
    "没反应",
    "卡住",
    "死循环",
    "推翻",
    "白做",
    "白干",
    "越改越",
    "更差",
    "更糟",
    "没生效",
    "不生效",
    "还是老问题",
    "仍然不对",
    "还是不",
    "没变",
    "误判",
    "误导",
    "方向不对",
    "错了方向",
    "理解错",
    "理解有误",
    "不对吧",
    "不对啊",
    "这不对",
    "这样不对",
    "重新执行",
    "重新设计",
    "重新论证",
    "重新分析",
    "重新检查",
    "重新弄",
    "重新搞",
    "重新来",
    "重新",
    "为什么还没有",
    "为什么还没",
    "为什么没有解决",
    "怎么还没有",
    "怎么还没",
    "还没解决",
    "仍然没有",
    "卡住了",
    "没看到你的输出",
    "看不到输出",
    "没有输出",
    "无输出",
    "并不构成",
    "不构成",
    "不是那样",
    "不是这样",
    "搞反",
    "白费",
];

/// 弱打回词(宽口径):含双义词(问题/缺/漏…),可能是同域新问题报告。
const WEAK_PUNCH_WORDS: &[&str] = &[
    "有问题",
    "少了",
    "漏了",
    "漏掉",
    "缺",
    "再检查",
    "再查一下",
    "弄错",
    "问题依旧",
    "没到位",
    "没落实",
    "不是",
    "没做完",
    "怎么还",
    "怎么还是",
    "仍然",
    "依然",
    "依旧",
];

/// 疑似推进词:未命中打回词但含"再/继续修"类推进语义,单独成桶。
const FORCE_WORDS: &[&str] = &[
    "再跑",
    "再试",
    "再验证",
    "再来",
    "接着",
    "继续排查",
    "继续修",
    "继续改",
    "再加",
    "再确认",
    "还有",
    "另外一个",
    "补充一下",
    "另外",
    "再往下",
    "继续做",
    "继续完成",
    "继续分析",
    "继续调查",
];

/// 自澄清前缀:用户发现自己误解(剔除,非打回)。
const SELF_CLARIFY_PREFIX: &[&str] = &[
    "是",
    "我说错",
    "我误",
    "我明",
    "明白了",
    "是我",
    "算我",
    "我自己",
];

/// 新域报告前缀:同域新问题报告(不视为对上次收尾质量不满)。
const NEW_DOMAIN_PREFIX: &[&str] = &[
    "我看到",
    "我发现",
    "现在的",
    "当前",
    "关于",
    "我注意到",
    "刚才看到",
    "再补充一个",
];

/// 分类:判定一条 user 输入是否为"打回"。
///
/// 规则顺序(与基线扫描 `tools/_punchback_baseline.py` 同构,含口径改进):
/// 1. 剔除零宽/空白;
/// 2. 精确承接词 → None;
/// 3. 自澄清 / 新域报告前缀 → None;
/// 4. 命中 强/弱/疑似 词表(子串匹配)→ 对应类;
/// 5. 长度 < 4(无命中时)→ None。
///
/// 注:与 python 基线不同,此处**先命中词表再判长度**,使"不对"/"不是"等短强打回
/// 不再被长度裁剪误剔(口径改进,量化影响极小)。
pub fn classify_punchback(text: &str) -> PunchbackClass {
    let trimmed = text.replace(['\u{2028}', '\r'], "");
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        return PunchbackClass::None;
    }
    let n = trimmed.replace(' ', "").chars().count();
    if n < 2 {
        return PunchbackClass::None;
    }
    if ACK_SET.contains(&trimmed) {
        return PunchbackClass::None;
    }
    // 剔除前缀(先于词表命中,防假阳性)。
    if SELF_CLARIFY_PREFIX.iter().any(|p| trimmed.starts_with(p)) {
        return PunchbackClass::None;
    }
    if NEW_DOMAIN_PREFIX.iter().any(|p| trimmed.starts_with(p)) {
        return PunchbackClass::None;
    }
    // 词表命中(子串),优先级 强 > 弱 > 疑似。
    if STRONG_PUNCH_WORDS.iter().any(|w| trimmed.contains(w)) {
        return PunchbackClass::Strong;
    }
    if WEAK_PUNCH_WORDS.iter().any(|w| trimmed.contains(w)) {
        return PunchbackClass::Weak;
    }
    if FORCE_WORDS.iter().any(|w| trimmed.contains(w)) {
        return PunchbackClass::Force;
    }
    if n < 4 {
        return PunchbackClass::None;
    }
    PunchbackClass::None
}

/// 会话级埋点记录。
#[derive(Debug, Serialize)]
struct PunchbackRecord {
    ts_ms: u64,
    session: String,
    turn_input: String, // 截断预览,避免膨胀
    class: PunchbackClass,
}

/// 转意(append)一条 turn 埋点到会话级文件。失败返回 Err(非关键路径,调用方忽略)。
pub fn record_turn(
    sessions_dir: &Path,
    session_id: &str,
    turn_input: &str,
    class: PunchbackClass,
) -> std::io::Result<()> {
    use std::io::Write;
    let path = session_punchback_path(sessions_dir, session_id);
    let preview: String = turn_input.chars().take(96).collect();
    let record = PunchbackRecord {
        ts_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        session: session_id.to_string(),
        turn_input: preview,
        class,
    };
    let json = serde_json::to_string(&record)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{json}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cls(s: &str) -> PunchbackClass {
        classify_punchback(s)
    }

    #[test]
    fn strong_qaulifies_examples() {
        // 从基线扫描中标定的真·强打回
        assert_eq!(
            cls("重新执行#2回测验证盘整背驰1买或1卖是否适合立即进场"),
            PunchbackClass::Strong
        );
        assert_eq!(cls("代码已经根据这些问题做了修复为什么还没有解决？是不是编译的新版本二进制不包含这些改动吗？"), PunchbackClass::Strong);
        assert_eq!(
            cls("你卡住了吗怎么到现在都没有看到你的输出"),
            PunchbackClass::Strong
        ); // "卡住"
        assert_eq!(
            cls("在我看来04:25并不构成底分型，是否分型识别过于宽松？"),
            PunchbackClass::Strong
        );
        assert_eq!(
            cls("这个方向到了架构层，你先重新设计方案"),
            PunchbackClass::Strong
        );
        assert_eq!(cls("仍然没有解决"), PunchbackClass::Strong);
        assert_eq!(cls("不对"), PunchbackClass::Strong); // 短强打回不被长度裁剪
    }

    #[test]
    fn weak_qaulifies_ambiguous() {
        assert_eq!(cls("结果缺少了关键字段"), PunchbackClass::Weak); // "缺"
        assert_eq!(cls("你漏掉了这部分没到位"), PunchbackClass::Weak); // "漏掉"/"没到位"
    }

    #[test]
    fn force_is_separate_bucket() {
        assert_eq!(cls("还有另外一个问题需要排查"), PunchbackClass::Force); // "还有"/"另外一个"
        assert_eq!(cls("继续修复才能收尾"), PunchbackClass::Force); // "继续修"
    }

    #[test]
    fn self_clarify_excluded() {
        assert_eq!(
            cls("是我误认为没解决，被热升级提示误导了"),
            PunchbackClass::None
        );
        assert_eq!(cls("明白了，是编译版本的问题"), PunchbackClass::None);
    }

    #[test]
    fn new_domain_report_excluded() {
        assert_eq!(
            cls("我看到图表上有些线段内部满足中枢条件但没有绘制出中枢，是不是BUG"),
            PunchbackClass::None
        );
        assert_eq!(
            cls("关于中枢的定义代码实现和原文有什么区别？"),
            PunchbackClass::None
        );
    }

    #[test]
    fn ack_and_short_excluded() {
        assert_eq!(cls("好"), PunchbackClass::None);
        assert_eq!(cls("好的，继续"), PunchbackClass::None);
        assert_eq!(cls("谢谢"), PunchbackClass::None);
        assert_eq!(cls("abc"), PunchbackClass::None);
        assert_eq!(cls(""), PunchbackClass::None);
    }

    #[test]
    fn known_false_positive_of_shi_bu_shi() {
        // 已知残留误判(文档 §6.1.3):"是不是"可命中 STRONG 的"不是",验收按 x0.85 修正。
        assert_eq!(
            cls("为什么现在的上下文缓存命中率这么低？是不是以前的机制被破坏了？"),
            PunchbackClass::Strong
        );
    }
}

// ════════════════════════════════════════════════════════════════════
// 进度信号投影(2026-10-27,P0-2):把 tool history 投影为「任务是否在推进」
// 的客观信号,供迭代/墙钟催缴做门控。
//
// 历史:本文件曾承载收敛契约 P1 的声明解析 / evidence 校验 / 打回
// (soft1/soft2 15K/25K + user 角色注入)。实证该契约被模型当成「用户在催
// 收尾」,在任务中途停下反问;惩罚性 evidence 校验还诱导过编造锚点。
// 2026-10-26 默认停用,2026-10-27 删除死路径,只保留本节的纯函数进度判据。
//
// 模块边界:纯函数,零 I/O;tool history → ToolOutcome 的投影由调用方
// conversation.rs 负责。
// ════════════════════════════════════════════════════════════════════

/// 运行时可采集的客观进度信号类型(P0 两档)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    /// 任意工具成功且输出形如测试通过。
    TestSuccess,
    /// 文件写入类工具成功。
    FileProduced,
}

/// 触发收敛催缴所需的**最少连续停滞迭代数**。
///
/// 轮次/耗时只是量级门;连续若干迭代无任何落盘或测试通过信号,才判定停滞。
/// 有进展即清零 —— 正在写文件/跑测试的长任务不该被催收尾。
pub const CONVERGENCE_MIN_STAGNANT_ITERS: u32 = 2;

/// tool history 中一次工具结果的轻量投影输入(由 conversation.rs 映射,
/// 本模块不依赖 session/ContentBlock 类型,保持纯函数可单测)。
#[derive(Debug, Clone, Copy)]
pub struct ToolOutcome<'a> {
    pub tool_name: &'a str,
    pub is_error: bool,
    pub output: &'a str,
}

/// 文件写入类工具判定(宽松子串):write/edit/replace/patch/rename/append。
fn is_file_write_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["write", "edit", "replace", "patch", "rename", "append"]
        .iter()
        .any(|k| n.contains(k))
}

/// 测试成功判定(宽松):排除真实失败("N failed",N>0),接受成功形态
/// (含 "passed" / "passing" / "all tests ok" / "test result: ok")。
/// 注:Rust 真实成功输出形如 "test result: ok. 11 passed; 0 failed" ——
/// "0 failed" 是成功信号而非失败,故不能简单用 `contains("failed")` 排除。
fn looks_like_test_success(output: &str) -> bool {
    let o = output.to_ascii_lowercase();
    // 真实失败:含 "failed" 但非 "0 failed"/"0 failures"(如 "1 failed"/"2 failed")。
    let real_failure = o.contains("failed")
        && !o.contains("0 failed")
        && !o.contains("0 failures")
        && !o.contains("passed, 0 failed");
    !real_failure
        && (o.contains("passed")
            || o.contains("passing")
            || o.contains("all tests ok")
            || o.contains("test result: ok"))
}

/// 单条工具结果 → 进度信号集合(供 conversation.rs 逐条投影后判断是否停滞)。
#[must_use]
pub fn project_one(outcome: ToolOutcome<'_>) -> Vec<SignalKind> {
    let mut v = Vec::new();
    if outcome.is_error {
        return v;
    }
    if is_file_write_tool(outcome.tool_name) {
        v.push(SignalKind::FileProduced);
    }
    if looks_like_test_success(outcome.output) {
        v.push(SignalKind::TestSuccess);
    }
    v
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn outcome<'a>(tool: &'a str, err: bool, out: &'a str) -> ToolOutcome<'a> {
        ToolOutcome {
            tool_name: tool,
            is_error: err,
            output: out,
        }
    }

    #[test]
    fn project_one_maps_file_write_and_test_success() {
        // 文件写成功 → FileProduced
        assert_eq!(
            project_one(outcome("write_file", false, "已写入 3 个文件")),
            vec![SignalKind::FileProduced]
        );
        // bash 测试通过 → TestSuccess
        assert_eq!(
            project_one(outcome(
                "bash",
                false,
                "test result: ok. 11 passed; 0 failed"
            )),
            vec![SignalKind::TestSuccess]
        );
        // 错误结果 → 空(不产生信号)
        assert!(project_one(outcome("write_file", true, "permission denied")).is_empty());
        // 失败输出含 failed → 不判 TestSuccess
        assert!(project_one(outcome("bash", false, "2 failed; 0 passed")).is_empty());
        // 读类工具成功 → 不产生信号
        assert!(project_one(outcome("read_file", false, "内容")).is_empty());
    }
}
