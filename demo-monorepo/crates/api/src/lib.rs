//! demo-api:API 服务层,引用 core + app。
//!
//! 设计意图(供 LSP 跨模块验证):
//! - 本文件是 monorepo 的"叶子/入口",**不被任何其他 crate 引用**。
//!   LSP 引用计数为 0 → 应排在 repomap 最末(或不在 budget 内)。
//! - 只消费 core/app,不新增被引用符号。

use demo_app::{quote_id, settle_order};
use demo_core::{apply_discount, OrderItem};

/// 处理一个订单请求(入口函数,无人引用)。
pub fn handle_order(items: &[OrderItem], discount_pct: f64) -> String {
    let settled = settle_order(items, discount_pct);
    format!("{}: {settled}", quote_id(1))
}

/// 计算优惠后的小计(入口辅助,无人引用)。
pub fn subtotal_after(items: &[OrderItem], pct: f64) -> f64 {
    let raw: f64 = items.iter().map(|i| i.price * i.quantity as f64).sum();
    apply_discount(raw, pct)
}
