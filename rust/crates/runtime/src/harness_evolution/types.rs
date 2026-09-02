//! harness_evolution 类型定义(design-gaps #2 self-evolving harness)。
//!
//! 数据模型与容量控制遵循
//! `docs/2026-07-24-p3-self-evolving-harness-design.md` §3:
//! - [`HarnessEdit`] 对应 dynamic_sections 中的一个可编辑段(L1 Guidance);
//! - 3 状态机 Candidate / Active / Retired;
//! - Active 上限 10 条、Candidate 上限 20 条、Retired 上限 50 条(LRU);
//! - 单条 content 上限 500 chars、全量注入总 token < 1.5K。

use serde::{Deserialize, Serialize};

/// 持久化的 harness edit,对应 dynamic_sections 中的一个可编辑段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessEdit {
    /// 唯一标识,格式:`edit-{timestamp}-{short_hash}`
    pub id: String,
    /// pathology 签名(来自 WeaknessMiner,用于质量多样性分桶)
    pub pathology: String,
    /// edit 内容(注入到 dynamic_sections 的文本)
    pub content: String,
    /// 状态(3 状态机)
    pub status: EditStatus,
    /// 来源:规则式 or LLM
    pub source: EditSource,
    /// 统计:验证次数
    pub verify_count: u32,
    /// 统计:成功次数(success_rate = success_count / verify_count)
    pub success_count: u32,
    /// 创建时间(ms since epoch)
    pub created_at: i64,
    /// 最后验证时间
    pub last_verified_at: Option<i64>,
    /// 提议来源的推理(规则式为模式名,LLM 为 reasoning)
    pub proposer_reasoning: String,
    /// simhash(用于去重,复用 decision_log::compute_simhash)
    pub similarity_hash: i64,
    /// 退役原因(`Retired` 时展示,如"significant degradation")
    pub retire_reason: Option<String>,
}

impl HarnessEdit {
    /// 成功率的辅助计算:成功次数 / 验证次数;未验证时为 0.0。
    #[must_use]
    pub fn success_rate(&self) -> f64 {
        if self.verify_count == 0 {
            0.0
        } else {
            f64::from(self.success_count) / f64::from(self.verify_count)
        }
    }
}

/// Edit 状态机(3 状态)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditStatus {
    /// 候选:刚提议,等待验证
    Candidate,
    /// 生效中:通过门控,正在注入 dynamic_sections
    Active,
    /// 已退役:未通过门控或 success_rate 衰减(统一表示"不再使用")
    Retired,
}

impl EditStatus {
    /// 数据库字符串表示。
    #[must_use]
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Candidate => "Candidate",
            Self::Active => "Active",
            Self::Retired => "Retired",
        }
    }

    /// 从数据库字符串解析;非法值返回 `None`。
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "Candidate" => Some(Self::Candidate),
            "Active" => Some(Self::Active),
            "Retired" => Some(Self::Retired),
            _ => None,
        }
    }
}

/// Edit 来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditSource {
    /// 规则式匹配(预定义模式)
    RulePattern,
    /// LLM 生成(Phase 3 扩展)
    LlmProposer,
    /// 已验证方案提炼(MVP-C1):decision_log 中 verification_result=Confirmed
    /// 的 applied_solution 提炼为规则——已被真实修复验证,比硬编码规则更可靠。
    VerifiedSolution,
}

impl EditSource {
    #[must_use]
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::RulePattern => "RulePattern",
            Self::LlmProposer => "LlmProposer",
            Self::VerifiedSolution => "VerifiedSolution",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "RulePattern" => Some(Self::RulePattern),
            "LlmProposer" => Some(Self::LlmProposer),
            "VerifiedSolution" => Some(Self::VerifiedSolution),
            _ => None,
        }
    }
}

/// Evolution 配置。MVP(规则式)默认零 LLM 调用。
#[derive(Debug, Clone, PartialEq)]
pub struct EvolutionConfig {
    /// 验证窗口:候选 edit 需要观察 N 个 turn
    pub validation_window: usize,
    /// 显著性测试阈值(alpha)
    pub significance_alpha: f64,
    /// success_rate 晋升阈值
    pub promote_threshold: f64,
    /// success_rate 回滚阈值
    pub rollback_threshold: f64,
    /// evolution 触发间隔(每 N turn)
    pub evolution_interval: usize,
    /// 失败聚类最少出现次数(过滤噪声)
    pub min_occurrences: usize,
    /// 每次最大提议数
    pub max_proposals: usize,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            validation_window: 10,
            significance_alpha: 0.05, // z > 1.96
            promote_threshold: 0.7,
            rollback_threshold: 0.3,
            evolution_interval: 10,
            min_occurrences: 2,
            max_proposals: 3,
        }
    }
}

/// Weakness mining 的输出 — 一个需要 Proposer 处理的失败模式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeaknessSignal {
    /// pathology 签名(失败聚类 label)
    pub pathology: String,
    /// 样本错误消息(最多 5 条)
    pub sample_errors: Vec<String>,
    /// 窗口内出现次数
    pub occurrence_count: u32,
    /// 关联的 turn_id 列表
    pub related_turns: Vec<String>,
}

/// 单轮 evolution 的摘要报告(供日志 / CLI 展示)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvolutionReport {
    /// 检测到的 weakness 数
    pub weaknesses_count: usize,
    /// 本轮新增提案数
    pub proposals_count: usize,
    /// 本轮晋升为 Active 的 edit 数
    pub promoted_count: usize,
    /// 本轮退役的 edit 数
    pub retired_count: usize,
    /// 已跳过(去重 / 规则未命中)数
    pub skipped_count: usize,
}

/// 两重门控验证结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationOutcome {
    /// 晋升为 Active
    Promoted,
    /// 继续保持 Candidate,等待更多数据
    StillCandidate(String),
    /// 退役,标记为 Retired
    Retired(String),
}

/// Archive 统计(供 CLI `claw harness stats`)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArchiveStats {
    pub total: u64,
    pub active: u64,
    pub candidate: u64,
    pub retired: u64,
    /// 平均 success_rate(仅 Active;无 Active 时为 0.0)
    pub avg_active_success_rate: f64,
    /// 规则来源占比
    pub rule_sourced: u64,
    pub llm_sourced: u64,
    /// 已验证方案提炼来源(MVP-C1)
    pub verified_solution_sourced: u64,
}
