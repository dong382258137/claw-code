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

    #[test]
    fn soft_thresholds_widens_for_readonly_turns() {
        // 干活轮(写过文件/测试通过):沿用原阈值。
        assert_eq!(
            soft_thresholds(false),
            (CONVERGENCE_SOFT1_OUTPUT, CONVERGENCE_SOFT2_OUTPUT)
        );
        // 只读侦察轮:显著放宽,避免把"正在检索"误判为"停滞"。
        assert_eq!(
            soft_thresholds(true),
            (
                CONVERGENCE_SOFT1_OUTPUT_READONLY,
                CONVERGENCE_SOFT2_OUTPUT_READONLY
            )
        );
        // 放宽方向由上面两组 assert_eq! 的取值本身保证(40K/60K vs 15K/25K),
        // 无需再写常量不等式断言(clippy::assertions_on_constants)。
    }

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
// 收敛契约 P1 窄版(2026-09-05,§5.1):收敛声明解析 + evidence 事实校验。
//
// 模块边界(与 docs §4 主权分离一致):
// - 本文件只含**纯函数**:入参为模型文本 + 运行时已采集的信号集合
//   (`available: &[SignalKind]`),与具体 ToolResult/session 类型解耦,可单测。
// - tool history → SignalKind 的投影(哪些信号在本会话真实存在)由调用方
//   conversation.rs 负责(步骤 3 接线),本模块不做 I/O。
//
// 宽松校验(§5.1):claimed_complete=true 时,evidence 能映射出至少一种
// 信号且该信号存在于 available 集合 → Verified;运行时无任何信号可参照
// (available 为空) → 放行(不误伤无测试类任务);强校验(≥2 类多样性、
// 子项覆盖)延后,随打回率数据驱动收紧。
// ════════════════════════════════════════════════════════════════════

/// 收敛声明起始标记(模型输出格式见 docs §4.2)。
pub const CONVERGENCE_MARKER: &str = "[收敛声明]";

/// 解析后的收敛声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergenceDeclaration {
    /// 任务目标一句话。
    pub goal: String,
    /// 声称任务是否完成。
    pub claimed_complete: bool,
    /// 支撑声称的客观信号描述(如 "regression 11 passed, exit 0")。
    pub evidence: String,
    /// 结论列表(≤5 条)。
    pub conclusions: Vec<String>,
    /// 未完成项列表(空 = 无)。
    pub todos: Vec<String>,
    /// 是否可以停止(可与 claimed_complete 拆分:有结论但需用户裁决)。
    pub can_stop: bool,
}

/// 运行时可采集的客观信号类型(§6.2 注册表 P0 两档;其余延后)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    /// Bash stdout 含 "N passed" 且 exit_code=0。
    TestSuccess,
    /// 文件产物已生成(write_file 等 is_error=false + 文件存在)。
    FileProduced,
}

impl SignalKind {
    /// 稳定的调试/序列化名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SignalKind::TestSuccess => "test_success",
            SignalKind::FileProduced => "file_produced",
        }
    }
}

/// evidence 文本 → 期望信号类型的关键词映射(宽松,不区分大小写)。
const TEST_SIGNAL_WORDS: &[&str] = &[
    "passed",
    "pass",
    "exit 0",
    "exit_code=0",
    "通过",
    "成功",
    "success",
    "ok",
];
const FILE_SIGNAL_WORDS: &[&str] = &[
    "已写入",
    "写入",
    "已生成",
    "生成",
    "created",
    "wrote",
    "produced",
    "write_file",
    "文件已",
];

/// 声明校验裁定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceVerdict {
    /// 声称的证据信号在运行时 tool history 中存在 → 可放行。
    Verified,
    /// 声称完成但 evidence 为空,或映射出的信号在 available 中不存在 → 证据缺失。
    Missing,
    /// evidence 非空但无法映射到任何已知信号类型 → 由调用方按宽松规则放行。
    Unverifiable,
}

/// 宽松解析收敛声明:含 `[收敛声明]` 且能读到 claimed_complete 与 can_stop
/// 两个布尔键才返回 `Some`(缺任一键视为未按格式声明)。
#[must_use]
pub fn parse_convergence_declaration(text: &str) -> Option<ConvergenceDeclaration> {
    if !text.contains(CONVERGENCE_MARKER) {
        return None;
    }
    #[derive(PartialEq)]
    enum Sec {
        Goal,
        Evidence,
        Conclusions,
        Todos,
        Other,
    }
    let mut sec = Sec::Other;
    let mut d = ConvergenceDeclaration {
        goal: String::new(),
        claimed_complete: false,
        evidence: String::new(),
        conclusions: Vec::new(),
        todos: Vec::new(),
        can_stop: false,
    };
    let mut saw_complete = false;
    let mut saw_stop = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // bullet 判定必须在 `split_once(':')` **之前**:
        // ① 层级上 bullet 是上一节内容的延续,不是新的 key;
        // ② evidence/结论项常自带冒号(如 `- turn: iterations=22`),若先切冒号
        //    会得到 k="- turn" 落进 `_ => Sec::Other`,既丢掉该项,又把 sec 置错,
        //    导致其后的 bullet 也一并丢失。
        if let Some(item) = line.strip_prefix("- ") {
            let item = item.trim();
            if !item.is_empty() {
                match sec {
                    Sec::Conclusions => d.conclusions.push(item.to_string()),
                    Sec::Todos => d.todos.push(item.to_string()),
                    // BUG 修复:此前 evidence 的多行/列表写法被 `_ => {}` 吞掉,
                    // 解析出空串 → validate_evidence 判 Missing → 打回提示渲染成
                    // `evidence "(空)"`。agent 因此无法收尾、反复重发声明。
                    Sec::Evidence => {
                        if !d.evidence.is_empty() {
                            d.evidence.push_str("; ");
                        }
                        d.evidence.push_str(item);
                    }
                    _ => {}
                }
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            sec = match k {
                "goal" => {
                    d.goal = v.to_string();
                    Sec::Goal
                }
                "claimed_complete" => {
                    if let Some(b) = parse_bool(v) {
                        d.claimed_complete = b;
                        saw_complete = true;
                    }
                    Sec::Other
                }
                "evidence" => {
                    d.evidence = v.to_string();
                    Sec::Evidence
                }
                "can_stop" => {
                    if let Some(b) = parse_bool(v) {
                        d.can_stop = b;
                        saw_stop = true;
                    }
                    Sec::Other
                }
                "conclusions" => {
                    if !v.is_empty() && v != "无" {
                        d.conclusions.push(v.to_string());
                    }
                    Sec::Conclusions
                }
                "todos" => {
                    if !v.is_empty() && v != "无" {
                        d.todos.push(v.to_string());
                    }
                    Sec::Todos
                }
                _ => Sec::Other,
            };
        }
        // 其他键值/散行:忽略(容忍模型格式漂移,宽松解析)。
    }
    if !saw_complete || !saw_stop {
        return None;
    }
    Some(d)
}

/// 布尔值宽松解析:true/yes/是 → true;false/no/否 → false;其余 → None。
fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "是" | "1" => Some(true),
        "false" | "no" | "否" | "0" => Some(false),
        _ => None,
    }
}

/// 把 evidence 文本粗分类为期望信号类型(可命中多种,宽松含 "ok"/"success")。
#[must_use]
pub fn evidence_signal_kinds(evidence: &str) -> Vec<SignalKind> {
    let lower = evidence.to_ascii_lowercase();
    let mut out = Vec::new();
    if TEST_SIGNAL_WORDS.iter().any(|w| lower.contains(w)) {
        out.push(SignalKind::TestSuccess);
    }
    if FILE_SIGNAL_WORDS.iter().any(|w| lower.contains(w)) {
        out.push(SignalKind::FileProduced);
    }
    out
}

/// evidence 事实校验(纯规则,零 LLM)。
///
/// 宽松语义(§5.1):
/// - evidence 为空 → `Missing`;
/// - evidence 无法映射到任何信号类型 → `Unverifiable`(调用方可放行,防误伤);
/// - 映射出的信号与 `available` 无交集 → `Missing`(声称的证据不存在于 tool history);
/// - 有交集 → `Verified`。
///
/// 特例:`available` 为空(运行时未采到任何信号,如无测试的问答任务)→ 一律
/// `Verified`(不误伤;打回率超阈值后再收紧,数据驱动)。
#[must_use]
pub fn validate_evidence(
    decl: &ConvergenceDeclaration,
    available: &[SignalKind],
) -> EvidenceVerdict {
    if decl.evidence.trim().is_empty() {
        return EvidenceVerdict::Missing;
    }
    let kinds = evidence_signal_kinds(&decl.evidence);
    if kinds.is_empty() {
        return EvidenceVerdict::Unverifiable;
    }
    if available.is_empty() {
        return EvidenceVerdict::Verified;
    }
    if kinds.iter().any(|k| available.contains(k)) {
        EvidenceVerdict::Verified
    } else {
        EvidenceVerdict::Missing
    }
}

/// 收敛契约 P1 软阈值(output tokens,文档 §4.3):达到时注入声明提示。
pub const CONVERGENCE_SOFT1_OUTPUT: u64 = 15_000;
pub const CONVERGENCE_SOFT2_OUTPUT: u64 = 25_000;

/// **只读侦察轮**的软阈值。
///
/// 缺陷:写文件轮与本轮无任何进度信号的只读轮共用 15K/25K 阈值。但只读轮
/// (grep/read/bash 调查)天然烧 token 却**不落盘** —— `project_one` 只把
/// `file_produced` / `test_success` 记作进度,纯读取必然被判为"停滞"。实测
/// 长调研 turn(30K+ output、几十次检索、零写入)会被连续催缴收尾,而它正在
/// 正常工作。语义上「用写文件轮的阈值判定只读轮停滞」不成立。
///
/// 修正:按本 turn 是否产生过进度信号选阈值对 —— 只读轮用显著更宽的阈值,
/// 保留对真 runaway 的拦截,又不误伤正常调研。
pub const CONVERGENCE_SOFT1_OUTPUT_READONLY: u64 = 40_000;
pub const CONVERGENCE_SOFT2_OUTPUT_READONLY: u64 = 60_000;

/// 按「本 turn 是否为只读侦察轮」选取软阈值对 `(soft1, soft2)`。
///
/// 纯函数,零 env 依赖,可单测。`readonly=true` 用放宽阈值,`false` 用标准
/// 阈值。只读轮的定义见调用点:本 turn **调用过工具**但从未产生进度信号。
/// 纯推理轮(一次工具都没调)不算只读轮 —— 它保留按量催缴的既有护栏。
#[must_use]
pub const fn soft_thresholds(readonly: bool) -> (u64, u64) {
    if readonly {
        (
            CONVERGENCE_SOFT1_OUTPUT_READONLY,
            CONVERGENCE_SOFT2_OUTPUT_READONLY,
        )
    } else {
        (CONVERGENCE_SOFT1_OUTPUT, CONVERGENCE_SOFT2_OUTPUT)
    }
}

/// 触发收敛催缴所需的**最少连续停滞迭代数**。
///
/// 缺陷:原实现仅以 `turn_output >= 15K/25K` 触发,把"token 燃烧量"当作
/// "任务未完成"的代理 —— 但推理型长任务本就烧 token,且 turn 内仍在产出
/// `file_produced` / `test_success` 时,token 是在**推进**而非空转。按量催缴
/// 的结果是催一个正在干活的模型收尾(实测长会话被连续催缴多轮)。
///
/// 修正:token 阈值仍作为必要条件(便宜的量级门),另需**连续若干迭代无任何
/// 进度信号**才注入。有进展即清零。阈值 2 = 容忍单个"纯思考/纯读取"迭代,
/// 两个连续迭代毫无产出的落盘/测试信号才判定停滞。
pub const CONVERGENCE_MIN_STAGNANT_ITERS: u32 = 2;

/// 证据打回(缺失)提示的上限次数:超过则不再打回,交既有成本护栏兜底,防死循环。
pub const CONVERGENCE_MAX_PUNCHBACKS: u8 = 2;

/// 读取收敛契约 P1 开关:默认开启;env `CLAW_CONVERGENCE_P1` 为 "0" 时关闭
/// (一键关,热升级后按打回率数据决定是否全量开,见 docs §5.1)。
#[must_use]
pub fn convergence_enabled() -> bool {
    convergence_enabled_from_env(std::env::var("CLAW_CONVERGENCE_P1").ok().as_deref())
}

/// env 原始值 → 开关(纯函数,可单测;零 unsafe,适配 crate deny(unsafe_code))。
#[must_use]
pub fn convergence_enabled_from_env(v: Option<&str>) -> bool {
    !matches!(v, Some("0"))
}

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

/// 把 tool history 投影为运行时客观信号集合(§6.2 P0 两档;零 LLM)。
///
/// - 文件写入类工具成功 → `FileProduced`;
/// - 任意工具成功且输出像测试通过 → `TestSuccess`(工具名不限,覆盖
///   bash/cargo test/pytest 等;宽松原则,防误伤)。
#[must_use]
pub fn project_signals<'a, I: IntoIterator<Item = ToolOutcome<'a>>>(
    outcomes: I,
) -> Vec<SignalKind> {
    let mut out = Vec::new();
    for o in outcomes {
        if o.is_error {
            continue;
        }
        if is_file_write_tool(o.tool_name) && !out.contains(&SignalKind::FileProduced) {
            out.push(SignalKind::FileProduced);
        }
        if looks_like_test_success(o.output) && !out.contains(&SignalKind::TestSuccess) {
            out.push(SignalKind::TestSuccess);
        }
    }
    out
}

/// 单条工具结果 → 信号集合(供 conversation.rs 逐条投影后去重合并)。
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

/// 收敛声明提示模板(注入为 user 消息;零 LLM,纯文本)。
///
/// `first=true` 为软阈值 1(15K)首次提示;`false` 为软阈值 2(25K)二次
/// 强调(已多次提示仍未收敛,要求解释)。
///
/// 站7(2026-09-19)瘦身:保留结构化协议字段(解析依赖),压缩说明性文字,
/// soft1 由 567B → ~410B(-28%)、soft2 由 589B → ~430B(-27%)。协议段
/// (goal/claimed_complete/evidence/conclusions/todos/can_stop)是解析
/// 语义的事实现实来源,不可裁剪。
#[must_use]
pub fn convergence_prompt_text(first: bool) -> String {
    let header = if first {
        "本 turn 输出已较高,请在回复开头输出一次【收敛声明】(下方格式);\
         运行时校验 evidence 是否真实存在于本会话 tool history,通过即可收尾。"
    } else {
        "仍未收到可校验的收敛声明。若进行中,请列出剩余 todos 与尚未获得的\
         客观信号;可收尾则按格式声明。禁止用无证据的\"即将完成\"式文本推进。"
    };
    format!(
        "{header}\n\n[收敛声明]\ngoal: <任务目标一句话>\n\
         claimed_complete: true | false\n\
         evidence: <支撑声称的客观信号,如 \"regression 11 passed, exit 0\">\n\
         conclusions: <≤5 条,- 列表>\ntodos: <未完成项,- 列表;空则 \"无\">\n\
         can_stop: true | false\n\n\
         填写原则:未完成时 claimed_complete=false 并列出剩余 todo,\
         不会因声明非 true 而强停。"
    )
}

/// 成本警示 × 收敛契约合并注记(站7 合并去重)。
///
/// 收敛契约存续但 soft1 尚未注入且模型未响应时,把成本警示文本升格为
/// 「报成本 + 收敛声明格式」一条合并注记 —— 两系统独立渲染会让"高 output
/// 喊收敛"的同一意图同 turn 交替注入多条(基线实证:单 turn 最高 6 次成本
/// 警示 + 6 次催缴),合并后一条完成两个职能。协议字段与
/// [`convergence_prompt_text`] 一致,解析路径复用。
#[must_use]
pub fn merged_cost_warning(cost_warning: &str) -> String {
    format!(
        "{cost_warning}\n请输出一次【收敛声明】(下方格式),运行时将校验 \
         evidence 是否真实存在于本会话 tool history,通过即可收尾:\n\
         [收敛声明]\ngoal: <一句话>\n\
         claimed_complete: true | false\n\
         evidence: <客观信号,如 \"regression 11 passed, exit 0\">\n\
         conclusions: <≤5 条>\ntodos: <未完成项;空则 \"无\">\n\
         can_stop: true | false\n\
         未完成时 claimed_complete=false 并列出剩余 todo,不会因声明非 true 而强停。"
    )
}

/// 证据缺失打回提示:告知模型 evidence 引用的信号未在本会话 tool history 中找到。
#[must_use]
pub fn render_missing_hint(decl: &ConvergenceDeclaration) -> String {
    let goal = if decl.goal.trim().is_empty() {
        "(未声明)".to_string()
    } else {
        decl.goal.clone()
    };
    let evidence = if decl.evidence.trim().is_empty() {
        "(空)".to_string()
    } else {
        decl.evidence.clone()
    };
    format!(
        "[runtime] 收敛声明未通过自动验证(evidence \\\"{evidence}\\\" 未能映射到本会话 \\\
         tool history 中的真实信号)。这**不是**对任务结果的否定:若任务确未完成,请直接如实 \\\
         声明 claimed_complete: false 并继续,无需构造 evidence —— 声称未完成时 evidence \\\
         允许为空。仅当你确实已产出可验证的客观产出(测试通过/文件写入)时才声明完成。\\\
         (goal: {goal})"
    )
}

#[cfg(test)]
mod convergence_p1_tests {
    use super::*;

    fn sample_decl() -> &'static str {
        "[收敛声明]\ngoal: 修复线段漏确认\nclaimed_complete: true\nevidence: regression 11 passed, exit 0\nconclusions:\n- 根因是分型宽松\n- 端点需校准\ntodos: 无\ncan_stop: true\n"
    }

    #[test]
    fn parses_valid_declaration() {
        let d = parse_convergence_declaration(sample_decl()).expect("should parse");
        assert_eq!(d.goal, "修复线段漏确认");
        assert!(d.claimed_complete);
        assert!(d.can_stop);
        assert!(d.evidence.contains("11 passed"));
        assert_eq!(d.conclusions.len(), 2);
        assert_eq!(d.conclusions[0], "根因是分型宽松");
        assert!(d.todos.is_empty());
    }

    /// BUG 回归:`evidence` 写成 YAML 列表(多行)时必须被完整收集。
    ///
    /// 缺陷:evidence 的多行写法此前落进 bullet 分支的 `_ => {}` 被丢弃,
    /// `d.evidence` 恒为空 → `validate_evidence` 判 `Missing` → 打回提示渲染
    /// `evidence "(空)"`。模型于是无法收尾、反复重发声明。现实样本见
    /// `inbox/1792064820141-agent-reported.md`。
    #[test]
    fn parses_evidence_written_as_bullet_list() {
        let txt = "[收敛声明]\n\
                   goal: 修 BUG-1\n\
                   claimed_complete: true\n\
                   evidence:\n\
                   - 二进制 Git SHA b9dad87d\n\
                   - cargo test 1 passed, exit 0\n\
                   conclusions:\n\
                   - 根因是 bullet 被吞\n\
                   todos: 无\n\
                   can_stop: true\n";
        let d = parse_convergence_declaration(txt).expect("should parse");
        assert_eq!(d.goal, "修 BUG-1");
        assert!(
            d.evidence.contains("b9dad87d"),
            "首项 evidence 丢失: {:?}",
            d.evidence
        );
        assert!(
            d.evidence.contains("1 passed"),
            "次项 evidence 丢失: {:?}",
            d.evidence
        );
        // 关键:evidence 非空,否则 validate_evidence → Missing → "(空)"
        assert!(!d.evidence.trim().is_empty());
        // conclusions 的 bullet 归属不被 evidence 抢走
        assert_eq!(d.conclusions.len(), 1);
        assert_eq!(d.conclusions[0], "根因是 bullet 被吞");
    }

    /// BUG 回归:bullet 内容自带冒号时不得被 `split_once(':')` 切成新 key。
    ///
    /// `- turn: iterations=22` 若先切冒号会得到 k="- turn",落进 `_ => Sec::Other`,
    /// 既丢掉该项又把 `sec` 置错,使其后所有 bullet 一并失效。
    #[test]
    fn bullet_containing_colon_stays_in_current_section() {
        let txt = "[收敛声明]\n\
                   goal: x\n\
                   claimed_complete: false\n\
                   evidence:\n\
                   - turn: iterations=22 elapsed=298s\n\
                   todos:\n\
                   - 待办: 继续验证\n\
                   can_stop: false\n";
        let d = parse_convergence_declaration(txt).expect("should parse");
        assert_eq!(
            d.evidence, "turn: iterations=22 elapsed=298s",
            "带冒号的 bullet 被误切为新 key"
        );
        assert_eq!(d.todos.len(), 1, "todos 的 bullet 丢失");
        assert_eq!(d.todos[0], "待办: 继续验证");
    }

    /// 单行 evidence 写法必须保持原行为(此次修复不能回归它)。
    #[test]
    fn single_line_evidence_still_parses() {
        let d = parse_convergence_declaration(sample_decl()).expect("should parse");
        assert_eq!(d.evidence, "regression 11 passed, exit 0");
    }

    #[test]
    fn missing_marker_returns_none() {
        assert!(parse_convergence_declaration("任务完成,无声明格式").is_none());
        assert!(parse_convergence_declaration("").is_none());
    }

    #[test]
    fn missing_boolean_key_returns_none() {
        let txt = "[收敛声明]\ngoal: x\nclaimed_complete: true\ncan_stop: 没写\n";
        assert!(parse_convergence_declaration(txt).is_none());
    }

    #[test]
    fn evidence_kinds_mapping() {
        assert_eq!(
            evidence_signal_kinds("regression 11 passed, exit 0"),
            vec![SignalKind::TestSuccess]
        );
        assert_eq!(
            evidence_signal_kinds("文件已写入 out/digest.md"),
            vec![SignalKind::FileProduced]
        );
        let both = evidence_signal_kinds("regression 11 passed, 已生成 report.csv");
        assert!(both.contains(&SignalKind::TestSuccess));
        assert!(both.contains(&SignalKind::FileProduced));
        assert!(evidence_signal_kinds("完成分析,无客观信号").is_empty());
    }

    #[test]
    fn validate_verified_and_missing() {
        let d = parse_convergence_declaration(sample_decl()).unwrap();
        assert_eq!(
            validate_evidence(&d, &[SignalKind::TestSuccess]),
            EvidenceVerdict::Verified
        );
        // available 只有 FileProduced,而 evidence 引用 TestSuccess → Missing
        assert_eq!(
            validate_evidence(&d, &[SignalKind::FileProduced]),
            EvidenceVerdict::Missing
        );
    }

    #[test]
    fn validate_empty_evidence_is_missing() {
        let mut d = parse_convergence_declaration(sample_decl()).unwrap();
        d.evidence.clear();
        assert_eq!(
            validate_evidence(&d, &[SignalKind::TestSuccess]),
            EvidenceVerdict::Missing
        );
    }

    #[test]
    fn validate_no_runtime_signal_is_lenient() {
        let d = parse_convergence_declaration(sample_decl()).unwrap();
        // available 为空 → 宽松放行,不误伤无测试任务。
        assert_eq!(validate_evidence(&d, &[]), EvidenceVerdict::Verified);
    }

    #[test]
    fn validate_unverifiable_evidence() {
        let mut d = parse_convergence_declaration(sample_decl()).unwrap();
        d.evidence = "已完成架构分析".to_string();
        assert_eq!(
            validate_evidence(&d, &[SignalKind::TestSuccess]),
            EvidenceVerdict::Unverifiable
        );
    }

    #[test]
    fn can_stop_false_with_complete_true() {
        // claimed_complete=true 但 can_stop=false(需用户裁决)→ 可解析出,由接线方决定是否停止。
        let txt = "[收敛声明]\ngoal: 等裁决方案 B\nclaimed_complete: true\nevidence: 回归通过\ntodos: 无\ncan_stop: false\n";
        let d = parse_convergence_declaration(txt).unwrap();
        assert!(d.claimed_complete);
        assert!(!d.can_stop);
    }

    // ── P1 接线层纯函数单测(步骤 4)──

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

    #[test]
    fn convergence_prompt_contains_marker_and_differs_by_tier() {
        let first = convergence_prompt_text(true);
        let second = convergence_prompt_text(false);
        assert!(first.contains("[收敛声明]"));
        assert!(first.contains("claimed_complete"));
        assert!(first.contains("evidence"));
        assert!(first != second, "首提与二次强调提示应不同");
        // 两种提示都不含会诱导提前收尾的强制口吻(均声明运行时不强停)。
        assert!(first.contains("不会因声明非 true 而强停"));
    }

    #[test]
    fn merged_cost_warning_embeds_declaration_format() {
        // 站7:成本警示 × 收敛契约合并注记 —— 保留成本原文前缀(测试与解析
        // 均复用 "成本警示" 前缀),追加完整收敛声明格式(解析路径可复用),
        // 未完成不诱导编造(明示 claimed_complete=false 不遭强停)。
        let cost = "[runtime] 成本警示:本 turn output 已超 30000 tokens(高燃烧)。建议立即收敛:...";
        let merged = merged_cost_warning(cost);
        assert!(
            merged.contains("[runtime] 成本警示"),
            "成本原文前缀必须保留"
        );
        assert!(merged.contains("[收敛声明]"));
        assert!(merged.contains("claimed_complete"));
        assert!(merged.contains("evidence"));
        assert!(merged.contains("can_stop"));
        assert!(
            merged.contains("不会因声明非 true 而强停"),
            "不得诱导编造完成态"
        );
        assert!(merged.len() >= cost.len());
    }

    #[test]
    fn render_missing_hint_carries_goal_and_evidence() {
        let d = parse_convergence_declaration(sample_decl()).unwrap();
        let hint = render_missing_hint(&d);
        assert!(hint.contains("未能映射到本会话"));
        assert!(hint.contains("修复线段漏确认")); // goal 原文
        assert!(hint.contains("11 passed")); // evidence 原文
                                             // 空 evidence 也应可渲染(不 panic)
        let mut empty = d.clone();
        empty.evidence.clear();
        assert!(render_missing_hint(&empty).contains("(空)"));
        // P4:打回文案应含"声明未完成时 evidence 允许为空"的宽松指引(消除编造动机)。
        assert!(hint.contains("claimed_complete: false"));
        assert!(hint.contains("允许为空"));
    }

    #[test]
    fn convergence_enabled_defaults_on_and_env_off() {
        // 纯函数:None → 开(默认)
        assert!(convergence_enabled_from_env(None));
        // "0" → 关
        assert!(!convergence_enabled_from_env(Some("0")));
        // 其他值 → 开
        assert!(convergence_enabled_from_env(Some("1")));
        assert!(convergence_enabled_from_env(Some("true")));
    }

    #[test]
    fn parser_tolerates_extra_noise_lines() {
        // 模型格式漂移:marker 前有前言、键间有散行 → 仍可解析(宽松)。
        let txt = "好的,以下是声明:\n[收敛声明]\ngoal: 修 bug\n一些过渡文字\nclaimed_complete: true\nevidence: test 1 passed\ncan_stop: true\n";
        let d = parse_convergence_declaration(txt).unwrap();
        assert_eq!(d.goal, "修 bug");
        assert!(d.claimed_complete);
        assert!(d.can_stop);
    }
}
