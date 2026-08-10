//! demo-app:业务应用层,引用 core + utils。
//!
//! 设计意图(供 LSP 跨模块验证):
//! - 本文件是"引用方",自身被 api 引用 → 中等重要性。
//! - 同时验证:一个文件既跨模块引用他人,也被他人引用时,
//!   LSP 计数只统计"指向别的文件"的引用位置。

use demo_core::{apply_discount, compute_total, format_total, OrderItem};
use demo_utils::{clamp, merge_prices};

/// 结算订单:计算总价 → 打折 → 格式化(被 api 引用)。
pub fn settle_order(items: &[OrderItem], discount_pct: f64) -> String {
    let total = compute_total(items);
    let discounted = apply_discount(total, discount_pct);
    format_total(clamp(discounted, 0.0, 100_000.0))
}

/// 生成报价单编号(被 api 引用)。
pub fn quote_id(base: u32) -> String {
    format!("Q-{base:05}")
}

/// 汇总价格表(内部使用,验证同一文件内跨模块引用计数)。
pub fn merged_prices(base: &[f64], extra: &[f64]) -> Vec<f64> {
    merge_prices(base, extra)
}
