//! demo-utils:通用工具函数,被 app 引用。
//!
//! 设计意图(供 LSP 跨模块验证):
//! - `merge_prices` 被 app 引用,中等重要性。
//! - `clamp` 被 `merge_prices` 内部引用 + app 直接引用。
//! - `legacy_prices` 仅被注释提及,无真实代码引用 —— 验证
//!   regex 会把注释/字符串中的名字误算成引用。

/// 将两份价格表合并(被 app 引用)。
pub fn merge_prices(base: &[f64], extra: &[f64]) -> Vec<f64> {
    let mut out = base.to_vec();
    for v in extra {
        out.push(clamp(*v, 0.0, 1_000_000.0));
    }
    out
}

/// 数值钳制到 [min, max](被 merge_prices 内部 + app 引用)。
pub fn clamp(v: f64, min: f64, max: f64) -> f64 {
    v.max(min).min(max)
}

/// 仅供文档示例:从未被代码引用,只出现在注释里。
/// 注意:legacy_prices 这个名字在本文件注释中出现过,
/// regex 子串匹配会把注释算成引用,造成假阳性。
pub fn legacy_prices(_n: usize) -> Vec<f64> {
    Vec::new()
}
