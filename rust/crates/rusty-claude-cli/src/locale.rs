//! 地区检测，用于本地化费用显示。
//!
//! 判定当前系统是否位于中国大陆地区，从而决定 TUI 费用显示
//! 使用 CNY 还是 USD。内部计费始终以 USD 存储，仅在显示层转换。

use sys_locale::get_locale;

/// 判定当前系统是否应使用 CNY 显示费用。
///
/// 判定规则：系统 locale 以 `zh-CN` 或 `zh-Hans` 开头时返回 `true`。
/// 其他 locale（包括 `zh-TW` / `zh-HK` / `zh-SG` 等）返回 `false`，
/// 因为这些地区通常不以 CNY 作为主计价货币。
///
/// 若系统无法获取 locale，回退到 `false`（USD 显示）。
#[must_use]
pub fn is_cny_region() -> bool {
    let locale = get_locale().unwrap_or_default();
    let lower = locale.to_ascii_lowercase();
    // 仅中国大陆（zh-CN）和新加坡（zh-SG）的简体中文用户使用 CNY 显示。
    // zh-SG 实际上以 SGD 计价，但为保守起见，仅匹配 zh-CN。
    lower.starts_with("zh-cn") || lower.starts_with("zh-hans-cn")
}

#[cfg(test)]
mod tests {
    use super::is_cny_region;

    #[test]
    fn returns_bool_without_panic() {
        // 仅验证函数可调用且返回 bool，不假设具体环境结果。
        let _ = is_cny_region();
    }
}
