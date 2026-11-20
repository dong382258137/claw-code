//! 模型能力分级 — 用于 subagent 任务路由。
//! 基于 ProviderCapabilityReport 但聚焦"任务可执行性"而非协议细节。
//!
//! 单模型：仅 deepseek-flash。Pro 已下线，不再参与路由或升级。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// 模型能力层级（粗粒度，用于任务路由）。
/// Ord: Budget < Standard < Flagship（声明顺序映射到 Ord 值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelTier {
    /// 轻量模型（deepseek-v4-flash）
    /// — 简单编辑、格式化
    Budget,
    /// 标准模型（deepseek-chat / deepseek-reasoner）
    /// — 常规任务
    Standard,
    /// 旗舰模型（deepseek-v4-pro）
    /// — 复杂推理、架构设计、诊断
    Flagship,
}

/// 任务复杂度需求 — 由调用方声明，coordinator 据此匹配模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskComplexity {
    /// 简单任务：单文件编辑、已知模式
    Simple,
    /// 诊断任务：根因定位、复杂调试 — 需要强推理能力
    Diagnostic,
    /// 架构决策：多方案评估、trade-off 分析
    Architectural,
}

impl TaskComplexity {
    /// 返回此复杂度所需的最低 ModelTier。
    /// Pro 已下线，flash 单模型承担全部复杂度，因此统一要求 Budget。
    #[must_use]
    pub const fn min_tier(self) -> ModelTier {
        match self {
            Self::Simple | Self::Diagnostic | Self::Architectural => ModelTier::Budget,
        }
    }
}

/// 按模型名推断能力层级。
///
/// Pro 已下线，历史 Pro ID 会先归一化到 flash，因此只返回 Budget/Standard。
#[must_use]
pub fn tier_for_model(model: &str) -> ModelTier {
    let canonical = super::resolve_model_alias(model);
    let lower = canonical.to_ascii_lowercase();

    if lower.contains("flash") {
        return ModelTier::Budget;
    }

    ModelTier::Standard
}

/// 检查模型是否满足任务复杂度需求。
#[must_use]
pub fn model_meets_complexity(model: &str, complexity: TaskComplexity) -> bool {
    tier_for_model(model) >= complexity.min_tier()
}

/// 模型升级记录 — 验证门禁失败时重试的更高级模型。
#[derive(Debug, Clone)]
pub struct UpgradeEntry {
    /// 升级目标模型名（如 "deepseek-v4-pro"）
    pub target_model: String,
    /// 成本倍数：升级后单次调用成本 ≈ 原成本 × cost_multiplier。
    /// P3 成本门禁预留，MVP 阶段不执行实际门禁计算。
    pub cost_multiplier: f64,
}

impl UpgradeEntry {
    fn new(target: &str, multiplier: f64) -> Self {
        Self {
            target_model: target.to_string(),
            cost_multiplier: multiplier,
        }
    }
}

/// 内置升级表（配置文件不存在时使用）。
///
/// Pro 已下线：升级链已彻底关闭，flash 不再升级到任何模型。
/// 配置文件 `~/.claw/model-upgrades.json` 仍可加载，但指向 Pro 的条目会被过滤。
fn default_upgrades() -> HashMap<String, UpgradeEntry> {
    HashMap::new()
}

/// 判断目标模型是否为已下线的 Pro。
fn is_retired_pro_model(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    lower == "pro" || lower.ends_with("-pro") || lower.ends_with("/pro")
}

/// 加载升级表（配置文件优先，回退到内置默认表）。
///
/// 配置文件路径：`~/.claw/model-upgrades.json`
/// 格式：`{ "upgrades": { "model_name": "target_model" } }`
fn upgrade_map() -> &'static HashMap<String, UpgradeEntry> {
    static CACHED: OnceLock<HashMap<String, UpgradeEntry>> = OnceLock::new();
    CACHED.get_or_init(|| {
        if let Some(map) = load_upgrade_config() {
            return map;
        }
        default_upgrades()
    })
}

fn load_upgrade_config() -> Option<HashMap<String, UpgradeEntry>> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    let path = home.join(".claw").join("model-upgrades.json");
    let content = std::fs::read_to_string(&path).ok()?;

    #[derive(serde::Deserialize)]
    struct UpgradeConfig {
        upgrades: HashMap<String, serde_json::Value>,
    }

    let config: UpgradeConfig = serde_json::from_str(&content).ok()?;
    let mut map = HashMap::new();
    for (key, value) in config.upgrades {
        let entry = match value {
            serde_json::Value::String(target) => UpgradeEntry::new(&target, 1.0),
            serde_json::Value::Object(obj) => {
                let target = obj
                    .get("target_model")
                    .or_else(|| obj.get("target"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)?;
                let cost_multiplier = obj
                    .get("cost_multiplier")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0);
                UpgradeEntry::new(&target, cost_multiplier)
            }
            _ => continue,
        };
        // Pro 已下线：即使配置文件里写了 pro，也不允许成为升级目标。
        if is_retired_pro_model(&entry.target_model) {
            continue;
        }
        map.insert(key, entry);
    }
    Some(map)
}

/// 查询模型的升级目标。
///
/// 返回 `Some(target_model)` 如果存在升级路径，
/// 返回 `None` 如果：
/// - 模型不在升级表中
/// - 模型已是最高层级（target == self）
#[must_use]
pub fn upgrade_model(model: &str) -> Option<String> {
    let canonical = super::resolve_model_alias(model);
    // 先用精确名查，再用规范化后的别名查
    let entry = upgrade_map()
        .get(model)
        .or_else(|| upgrade_map().get(&canonical));

    match entry {
        Some(entry)
            if entry.target_model != model
                && entry.target_model != canonical
                && !is_retired_pro_model(&entry.target_model) =>
        {
            Some(entry.target_model.clone())
        }
        _ => None,
    }
}

/// 返回升级后的成本倍数，用于成本估算。
/// 如果模型没有升级路径，返回 1.0（无增量）。
#[must_use]
pub fn upgrade_cost_multiplier(model: &str) -> f64 {
    let canonical = super::resolve_model_alias(model);
    upgrade_map()
        .get(model)
        .or_else(|| upgrade_map().get(&canonical))
        .map_or(1.0, |entry| entry.cost_multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_deepseek_models_correctly() {
        // Pro 已下线：历史 Pro ID 归一化后按 flash 计
        assert_eq!(tier_for_model("deepseek-v4-pro"), ModelTier::Budget);
        assert_eq!(tier_for_model("deepseek-v4-flash"), ModelTier::Budget);
        assert_eq!(tier_for_model("deepseek-flash"), ModelTier::Budget);
        // Prefixed forms
        assert_eq!(
            tier_for_model("deepseek/deepseek-v4-pro"),
            ModelTier::Budget
        );
        assert_eq!(
            tier_for_model("deepseek/deepseek-v4-flash"),
            ModelTier::Budget
        );
        // Standard tier
        assert_eq!(tier_for_model("deepseek-chat"), ModelTier::Standard);
        assert_eq!(tier_for_model("deepseek-reasoner"), ModelTier::Standard);
    }

    #[test]
    fn task_complexity_min_tiers() {
        // 单模型：所有复杂度统一由 flash 承担
        assert_eq!(TaskComplexity::Simple.min_tier(), ModelTier::Budget);
        assert_eq!(TaskComplexity::Diagnostic.min_tier(), ModelTier::Budget);
        assert_eq!(TaskComplexity::Architectural.min_tier(), ModelTier::Budget);
    }

    #[test]
    fn model_meets_complexity_validates_correctly() {
        // flash 单模型满足全部复杂度
        assert!(model_meets_complexity(
            "deepseek-flash",
            TaskComplexity::Diagnostic
        ));
        assert!(model_meets_complexity(
            "deepseek-v4-flash",
            TaskComplexity::Architectural
        ));
        assert!(model_meets_complexity(
            "deepseek-v4-flash",
            TaskComplexity::Simple
        ));
        assert!(model_meets_complexity(
            "deepseek-chat",
            TaskComplexity::Simple
        ));
    }

    #[test]
    fn upgrade_flash_returns_none_pro_retired() {
        // Pro 已下线，升级链彻底关闭
        assert_eq!(upgrade_model("deepseek-v4-flash"), None);
        assert_eq!(upgrade_model("deepseek-flash"), None);
    }

    #[test]
    fn upgrade_pro_returns_none_no_upgrade_path() {
        let upgraded = upgrade_model("deepseek-v4-pro");
        assert_eq!(upgraded, None);
    }

    #[test]
    fn upgrade_unknown_model_returns_none() {
        let upgraded = upgrade_model("unknown-model-v1");
        assert_eq!(upgraded, None);
    }

    #[test]
    fn upgrade_cost_multiplier_deepseek() {
        // 升级链已关闭，任何模型倍数都是 1.0
        assert_eq!(upgrade_cost_multiplier("deepseek-v4-flash"), 1.0);
        assert_eq!(upgrade_cost_multiplier("deepseek-flash"), 1.0);
        assert_eq!(upgrade_cost_multiplier("deepseek-v4-pro"), 1.0);
        assert_eq!(upgrade_cost_multiplier("unknown-model"), 1.0);
    }

    #[test]
    fn regression_flagship_models_no_upgrade() {
        // deepseek-v4-pro 已旗舰，不应再升级
        assert_eq!(upgrade_model("deepseek-v4-pro"), None);
    }
}
