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

/// 词表命中的**左邻**排除:这些字紧邻命中词之前时,表达的是取舍而非否定。
///
/// 只收「而不是」。「是不是」同样是误报(疑问句),但它是**已知残留误判**
/// (`convergence::tests::known_false_positive_of_shi_bu_shi` 显式固化,基线按
/// x0.85 修正),本模块不动它 —— 口径变更须连同基线一起改,不属于本次缺陷范围。
const PUNCH_LEFT_GUARDS: &[(&str, &[&str])] = &[
    // 「而不是」= 取舍("A 而不是 B"),不是对上一轮收尾的否定。
    ("不是", &["而"]),
];

/// 词表命中的**右邻**排除:命中词与这些字连成更长的词,语义已变。
const PUNCH_RIGHT_GUARDS: &[(&str, &[&str])] = &[
    // 缺陷 / 缺点 / 缺乏 —— 名词或"缺少"的另一种写法,不是「缺了东西」。
    ("缺", &["陷", "点", "乏"]),
    // 重新编译 / 重新安装 / 重新构建 / 重新划分 —— 常规操作动词,不是「要求重做」。
    ("重新", &["编译", "安装", "构建", "划分"]),
    // 无效项 / 无效数据 —— 指数据不合法,不是「你的修复无效」。
    ("无效", &["项", "数据", "条目"]),
];

/// `text` 是否以**未被否定/嵌入**的形式包含 `word`。
///
/// 词表是子串匹配,对多字词天然会把「更长的词」误判成打回证据:见
/// [`PUNCH_LEFT_GUARDS`] / [`PUNCH_RIGHT_GUARDS`]。这些嵌入形式在真实语料里
/// 占比很高 —— 实测 222 条打回埋点中 19 条被判 `strong`,其中 10 条的命中位置
/// **全部**落在首个分句之后(即长任务指令里的偶然子串),并非对上轮收尾的否定。
///
/// 只排除**明确改变语义**的紧邻字,不做「必须在开头」之类的位置裁剪:打回可以
/// 出现在句中("我看了你的输出,根本不对"),按位置裁剪会漏掉真打回。
fn contains_punch_word(text: &str, word: &str) -> bool {
    let empty: &[&str] = &[];
    let left = PUNCH_LEFT_GUARDS
        .iter()
        .find(|(w, _)| *w == word)
        .map_or(empty, |(_, g)| *g);
    let right = PUNCH_RIGHT_GUARDS
        .iter()
        .find(|(w, _)| *w == word)
        .map_or(empty, |(_, g)| *g);
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let left_ok = !left.iter().any(|g| text[..start].ends_with(g));
        let right_ok = !right.iter().any(|g| text[end..].starts_with(g));
        if left_ok && right_ok {
            return true;
        }
        // 跳过当前命中,继续找下一个(同一词可能既被排除又被真正使用)。
        from = match word.chars().next() {
            Some(c) => start + c.len_utf8(),
            None => return false,
        };
    }
    false
}

/// 分类:判定一条 user 输入是否为"打回"。
///
/// 规则顺序(与基线扫描 `tools/_punchback_baseline.py` 同构,含口径改进):
/// 1. 剥离外壳层注入块([`crate::task_state::strip_injected_prefix`]);
/// 2. 剔除零宽/空白;
/// 3. 精确承接词 → None;
/// 4. 自澄清 / 新域报告前缀 → None;
/// 5. 命中 强/弱/疑似 词表(边界感知子串匹配)→ 对应类;
/// 6. 长度 < 4(无命中时)→ None。
///
/// 注:与 python 基线不同,此处**先命中词表再判长度**,使"不对"/"不是"等短强打回
/// 不再被长度裁剪误剔(口径改进,量化影响极小)。
///
/// # 为什么要先剥离注入块
///
/// Web 端把「计划模式指令 / Web 计划 / Web 待办」三类块**前置拼进 user 文本**
/// 再入库。这些块不是用户发言,但正文里常含「缺」「不是」等词 —— 不剥离就会被
/// 判成用户对上轮收尾的否定。实测 222 条打回埋点中 12 条正是此类
/// (正文形如 `[计划模式] …不能只在回复里写…`,被判 `weak`/`strong`)。
/// 剥离逻辑复用 `task_state` 的既有实现,不在此另立口径。
pub fn classify_punchback(text: &str) -> PunchbackClass {
    let stripped = crate::task_state::strip_injected_prefix(text);
    let normalized = stripped.replace(['\u{2028}', '\r'], "");
    let trimmed = normalized.trim();
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
    // 词表命中(边界感知子串),优先级 强 > 弱 > 疑似。
    if STRONG_PUNCH_WORDS
        .iter()
        .any(|w| contains_punch_word(trimmed, w))
    {
        return PunchbackClass::Strong;
    }
    if WEAK_PUNCH_WORDS
        .iter()
        .any(|w| contains_punch_word(trimmed, w))
    {
        return PunchbackClass::Weak;
    }
    if FORCE_WORDS.iter().any(|w| contains_punch_word(trimmed, w)) {
        return PunchbackClass::Force;
    }
    if n < 4 {
        return PunchbackClass::None;
    }
    PunchbackClass::None
}

/// 纯承接/推进词(精确匹配整条输入 → 视为「续接」而非新任务)。
///
/// P2(2026-10-27):`继续` 类输入此前只归入 ACK(None),无法量化"上一轮
/// 收尾后用户被迫续做"的比例。`is_resume_intent` 单独识别它,作为
/// 完成度不足的弱信号(与打回区分:续接是"没做完",打回是"做错了")。
const RESUME_EXACT: &[&str] = &[
    "继续",
    "继续吧",
    "继续做",
    "继续干",
    "继续推进",
    "接着",
    "接着做",
    "往下",
    "直接开始",
    "开始吧",
];

/// 续接前缀:短输入且以这些词开头,视为续做同一任务。
const RESUME_PREFIXES: &[&str] = &[
    "继续",
    "接着",
    "直接开始",
    "好的继续",
    "好的,继续",
    "好的，继续",
];

/// 判定一条 user 输入是否为「续接」(要求继续上一轮未完成的工作)。
///
/// 保守口径:仅短输入(≤40 字符)且精确命中/前缀命中续接词才计;长输入
/// 默认视为新任务,避免把"继续查一下 A、B、C…"这类新指令误计为续接。
#[must_use]
pub fn is_resume_intent(text: &str) -> bool {
    let normalized = text.replace(['\u{2028}', '\r'], "");
    let trimmed = normalized
        .trim()
        .trim_end_matches(['。', '!', '！', '?', '？', '.', ',', '，']);
    if trimmed.is_empty() || trimmed.chars().count() > 40 {
        return false;
    }
    RESUME_EXACT.contains(&trimmed)
        || RESUME_PREFIXES
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
}

/// 会话级埋点记录。
#[derive(Debug, Serialize)]
struct PunchbackRecord {
    ts_ms: u64,
    session: String,
    turn_input: String, // 截断预览,避免膨胀
    class: PunchbackClass,
    /// 是否为「续接」(P2):上一轮收尾后用户要求继续,而非新任务/打回。
    resume: bool,
}

/// 转意(append)一条 turn 埋点到会话级文件。失败返回 Err(非关键路径,调用方忽略)。
pub fn record_turn(
    sessions_dir: &Path,
    session_id: &str,
    turn_input: &str,
    class: PunchbackClass,
    resume: bool,
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
        resume,
    };
    let json = serde_json::to_string(&record)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{json}")
}

/// 监管注记类型(P2 效果埋点):区分进度播报/侦察护栏/充分性提醒。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NagKind {
    /// 迭代/墙钟进度播报。
    Progress,
    /// 侦察护栏/预算倒数/预算硬收口。
    Guard,
    /// 充分性判据已满足提醒。
    Sufficiency,
}

/// 单 turn 实际注入(占用仲裁槽并 flush)的监管注记统计。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TurnNagStats {
    pub progress: u32,
    pub guard: u32,
    pub sufficiency: u32,
    /// 首次注入所在迭代(0 = 本 turn 未注入任何注记)。
    pub first_iter: usize,
}

impl TurnNagStats {
    /// 记录一条实际注入的注记。
    pub fn record(&mut self, kind: NagKind, iterations: usize) {
        match kind {
            NagKind::Progress => self.progress = self.progress.saturating_add(1),
            NagKind::Guard => self.guard = self.guard.saturating_add(1),
            NagKind::Sufficiency => self.sufficiency = self.sufficiency.saturating_add(1),
        }
        if self.first_iter == 0 {
            self.first_iter = iterations;
        }
    }

    /// 本 turn 实际注入的注记总数。
    #[must_use]
    pub fn total(&self) -> u32 {
        self.progress
            .saturating_add(self.guard)
            .saturating_add(self.sufficiency)
    }
}

/// 单 turn 效果埋点记录(P2):用于计算"首次注记 → 收尾"的迭代距离。
#[derive(Debug, Serialize)]
struct TurnStatsRecord {
    ts_ms: u64,
    session: String,
    iterations: usize,
    nag_progress: u32,
    nag_guard: u32,
    nag_sufficiency: u32,
    first_nag_iter: usize, // 0 = 未注入
    output_tokens: u64,
}

/// 追加一条 turn 效果埋点到 `<session_id>-turn_stats.jsonl`。
///
/// 与 punchback 同位点、同生命周期;失败静默(非关键路径)。分析口径:
/// `iterations - first_nag_iter` = 注记注入后到收尾又跑了多少轮。
pub fn record_turn_stats(
    sessions_dir: &Path,
    session_id: &str,
    iterations: usize,
    stats: &TurnNagStats,
    output_tokens: u64,
) -> std::io::Result<()> {
    use std::io::Write;
    let path = sessions_dir.join(format!("{session_id}-turn_stats.jsonl"));
    let record = TurnStatsRecord {
        ts_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        session: session_id.to_string(),
        iterations,
        nag_progress: stats.progress,
        nag_guard: stats.guard,
        nag_sufficiency: stats.sufficiency,
        first_nag_iter: stats.first_iter,
        output_tokens,
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

    /// 回归(实测缺陷):词表子串匹配把**更长的词**误判成打回证据。
    ///
    /// 证据:inbox 提案 `1795807351104` trigger=punchback,该 turn 的真实 user
    /// 输入为「我认为你的建议的处理顺序是正确的，但是关于第三点…这个提案要 llm
    /// 去写，**而不是**要系统自动去判定…」。命中 `STRONG_PUNCH_WORDS` 的只有
    /// 「不是」二字,且它是「而不是」的一部分 —— 表达取舍,不是对上一轮收尾的否定。
    /// 该 turn 因此被判 `strong` → `punchback` 门控命中 → 生成一条无信息量的提案。
    ///
    /// 注:「是不是」疑问句是**已知残留误判**,由
    /// [`known_false_positive_of_shi_bu_shi`] 显式固化(基线按 x0.85 修正),
    /// 不在本次修复范围内 —— 改它须连同基线一起改。
    #[test]
    fn punch_words_embedded_in_longer_words_are_not_punchback() {
        // 「而不是」= 取舍,不是否定。
        assert_eq!(
            cls("这个提案要 llm 去写，而不是要系统自动去判定，都是有缺陷的"),
            PunchbackClass::None
        );
        // 「无效项」是名词(数据不合法),不是「你的修复无效」。
        assert_eq!(
            cls("对 15 条任务批量跑 validate，剔除无效项后再用作 pass^1 判据"),
            PunchbackClass::None
        );
        // 「重新编译 / 重新安装 / 重新划分」是常规动作,不是「要求重做上轮结论」。
        assert_eq!(
            cls("必要的时候可以自己做工具或者脚本启动自进化重新编译二进制再继续执行任务"),
            PunchbackClass::None
        );
        assert_eq!(
            cls("你的重新编译安装没有使用热升级功能？我看到现在运行的还是旧版本"),
            PunchbackClass::None
        );
        // 「缺陷」是名词,不是「缺了东西」。
        assert_eq!(
            cls("定位这个热升级缺陷的根因，先看退出路径"),
            PunchbackClass::None
        );
    }

    /// 排除只针对**紧邻**的语义改变字:真打回仍须判出,不能被排除规则放过。
    #[test]
    fn genuine_punchback_survives_boundary_guards() {
        // 「不是」独立使用(前无「是/而」)= 真否定。
        assert_eq!(cls("不是这样，你理解错了"), PunchbackClass::Strong);
        assert_eq!(cls("这不是我要的结果"), PunchbackClass::Strong);
        // 「重新」后接非动作名词 = 要求重做。
        assert_eq!(cls("你先重新设计方案"), PunchbackClass::Strong);
        assert_eq!(cls("重新执行#2回测验证"), PunchbackClass::Strong);
        // 「缺」后不接「陷/点/乏」= 真缺漏。
        assert_eq!(cls("结果缺少了关键字段"), PunchbackClass::Weak);
        // 「无效」后不接「项/数据/条目」= 真否定。
        assert_eq!(cls("这个修复无效，问题依旧"), PunchbackClass::Strong);
    }

    /// 回归(实测缺陷):外壳层注入块被当成用户发言参与打回判定。
    ///
    /// Web 端把「计划模式指令 / Web 计划 / Web 待办」三类块前置拼进 user 文本
    /// 再入库。这些块不是用户发言,但正文含「缺」「不是」等词。实测 222 条埋点
    /// 中 12 条正是此类:正文形如 `[计划模式] …不能只在回复里写…`、
    /// `[Web 计划…] 任务: …重新划分…`,被判 `weak`/`strong` → 误触发 punchback。
    #[test]
    fn injected_shell_blocks_are_not_punchback() {
        let plan_mode = "[计划模式] 当前会话处于计划模式：计划必须用工具登记，不能只在回复里写。\n\
                         - 尚无计划：先调用 create_plan 建立计划（任务理解、步骤、每步验收标准）\n\n\
                         这个方案你重新划分一下职责";
        assert_eq!(cls(plan_mode), PunchbackClass::None);
        let web_plan = "[Web 计划（用户在 Web 端维护，作为当前执行计划）]\n\
                        任务: 第一性原理回答并实施 5 项：①子智能体只读权限合理性 ②职责划分合理性\n\n\
                        先把第一项做完";
        assert_eq!(cls(web_plan), PunchbackClass::None);
        // 注入块剥离后剩下的正文若确实是打回,仍须判出 —— 剥离不等于免检。
        let plan_mode_then_punch = "[计划模式] 当前会话处于计划模式。\n\n\
                                     你上次改的完全不对，重新做";
        assert_eq!(cls(plan_mode_then_punch), PunchbackClass::Strong);
    }

    fn temp_sessions_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claw-convergence-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp sessions dir");
        dir
    }

    /// P2:续接与"新任务/打回"必须可区分,长指令不误计。
    #[test]
    fn resume_intent_is_separate_from_new_task() {
        for text in [
            "继续",
            "继续吧",
            "继续 B1 > B2 > B3",
            "直接开始。",
            "好的，继续",
            "接着做",
        ] {
            assert!(is_resume_intent(text), "应判为续接: {text}");
        }
        for text in [
            "好的",
            "谢谢",
            "不对",
            "帮我检查并修复登录接口的 401 问题",
            "继续之前那个任务，但这次先只做静态分析，不要改代码，也不要跑测试，也不要更新文档，只给出结论和文件行号",
        ] {
            assert!(!is_resume_intent(text), "不应判为续接: {text}");
        }
    }

    #[test]
    fn record_turn_persists_resume_flag() {
        let dir = temp_sessions_dir("punchback-resume");
        record_turn(&dir, "s1", "继续 B1", PunchbackClass::None, true).expect("record");
        let text =
            std::fs::read_to_string(session_punchback_path(&dir, "s1")).expect("read punchback");
        assert!(text.contains("\"resume\":true"), "resume 标记缺失: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P2:turn 效果埋点必须可回算「首次注记位置 → 收尾轮数」。
    #[test]
    fn record_turn_stats_appends_machine_readable_line() {
        let dir = temp_sessions_dir("turn-stats");
        let mut stats = TurnNagStats::default();
        stats.record(NagKind::Progress, 64);
        stats.record(NagKind::Guard, 70);
        assert_eq!(stats.total(), 2);
        record_turn_stats(&dir, "s1", 80, &stats, 12_345).expect("record stats");
        record_turn_stats(&dir, "s1", 10, &TurnNagStats::default(), 0).expect("record zero stats");
        let text =
            std::fs::read_to_string(dir.join("s1-turn_stats.jsonl")).expect("read turn stats");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "应追加两行: {text}");
        assert!(lines[0].contains("\"iterations\":80"), "记录: {}", lines[0]);
        assert!(
            lines[0].contains("\"nag_progress\":1"),
            "记录: {}",
            lines[0]
        );
        assert!(lines[0].contains("\"nag_guard\":1"), "记录: {}", lines[0]);
        assert!(
            lines[0].contains("\"first_nag_iter\":64"),
            "记录: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"output_tokens\":12345"),
            "记录: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("\"first_nag_iter\":0"),
            "记录: {}",
            lines[1]
        );
        let _ = std::fs::remove_dir_all(&dir);
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
