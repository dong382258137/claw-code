//! demo-core:核心业务领域类型,被 app/api 广泛引用。
//!
//! 设计意图(供 LSP 跨模块验证):
//! - `compute_total` / `apply_discount` 被 app 和 api 跨 crate 引用,
//!   高重要性文件 —— 应排在 repomap 前面。
//! - `format_total` vs `format_total_raw`:只有前者被真实引用。
//!   旧的 regex 子串匹配会因 `format_total_raw` 包含 `format_total`
//!   而误判它也被引用;LSP references 能精确区分两者。

/// 订单条目。
pub struct OrderItem {
    pub price: f64,
    pub quantity: u32,
}

impl OrderItem {
    pub fn new(price: f64, quantity: u32) -> Self {
        Self { price, quantity }
    }
}

/// 核心计算:订单总价 = 单价 × 数量 之和。
pub fn compute_total(items: &[OrderItem]) -> f64 {
    items.iter().map(|i| i.price * i.quantity as f64).sum()
}

/// 折扣应用(被 api 引用)。
pub fn apply_discount(total: f64, percent: f64) -> f64 {
    total * (1.0 - percent / 100.0)
}

/// 格式化金额,输出 `$xx.xx`(被 app 引用)。
pub fn format_total(total: f64) -> String {
    format!("${:.2}", total)
}

/// 伪格式化变体:`format_total` 的子串,但**从未被任何 crate 引用**。
/// regex 子串匹配会误判它被引用,LSP 不会。
pub fn format_total_raw(total: f64) -> String {
    format!("{total:.2}")
}

/// 辅助:检测订单是否为空(内部使用,无跨模块引用)。
fn is_empty(items: &[OrderItem]) -> bool {
    items.is_empty()
}
