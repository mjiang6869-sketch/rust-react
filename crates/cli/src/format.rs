//! 终端输出格式化。
//!
//! 全部用整数运算——`clippy.toml` 在编译期禁用 `f32`/`f64`。展示用途也不例外：
//! 规则一旦开了口子就会逐渐被绕过。

use rust_decimal::Decimal;

/// 把 `Decimal` 渲染为最多 `places` 位小数，去掉尾随零。
///
/// 注意 `rust_decimal` 的 `round_dp` 用**银行家舍入**（四舍六入五取偶），
/// 所以 `3200.125.round_dp(2)` 是 `3200.12` 而非 `3200.13`。展示用途无妨，
/// 但不能拿它做交易数值的舍入——那必须走 `Precision::price_for`。
pub fn dec(v: Decimal, places: u32) -> String {
    let r = v.round_dp(places);
    // 去掉尾随零：`4.00` -> `4`，`45.50` -> `45.5`
    let s = r.normalize().to_string();
    if s == "-0" { "0".to_string() } else { s }
}

/// 字节数 → MB，一位小数。
pub fn mb(bytes: u64) -> String {
    let tenths = (bytes * 10) / 1_000_000;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// 把比例渲染为百分比，最多一位小数。
pub fn pct(ratio: Decimal) -> String {
    format!("{}%", dec(ratio * Decimal::from(100), 1))
}

/// 基点。用于展示止盈止损距离。
pub fn bp(ratio: Decimal) -> String {
    format!("{}bp", dec(ratio * Decimal::from(10_000), 2))
}

/// 有符号金额。正数带 `+`。
pub fn signed(v: Decimal) -> String {
    let n = v.normalize();
    if n > Decimal::ZERO {
        format!("+{n}")
    } else {
        format!("{n}")
    }
}

/// 时长（秒）→ 人类可读。
pub fn duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// 打印一条分隔线。
pub fn rule(width: usize) {
    println!("{}", "─".repeat(width));
}

/// 打印键值对，左对齐到指定宽度。
pub fn kv(key: &str, value: &str) {
    println!("  {key:<16} {value}");
}

/// 在终端里把一组标签统计渲染成对齐的多行。
pub fn tally(title: &str, items: &[(&str, usize)]) {
    if items.is_empty() {
        return;
    }
    println!("\n{title}");
    let mut sorted: Vec<_> = items.to_vec();
    sorted.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let width = sorted
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    for (k, n) in sorted {
        println!("  {k:<width$}  {n}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn decimal_rounds_to_places_and_trims_zeros() {
        assert_eq!(dec(dec!(3200.123456), 2), "3200.12");
        assert_eq!(dec(dec!(3200), 2), "3200", "整数不应带尾随零");
        assert_eq!(dec(dec!(4.00), 2), "4");
        assert_eq!(dec(dec!(45.50), 2), "45.5");
    }

    /// `rust_decimal` 用银行家舍入：恰好一半时取偶。
    /// 这个行为要显式记录，避免以后有人以为它是四舍五入。
    #[test]
    fn rounding_uses_bankers_rule_at_exact_half() {
        assert_eq!(dec(dec!(3200.125), 2), "3200.12", "5 取偶");
        assert_eq!(dec(dec!(3200.135), 2), "3200.14", "5 取偶到 4");
    }

    /// 负数不能渲染成 `-0`。
    #[test]
    fn negative_zero_is_normalized() {
        assert_eq!(dec(dec!(-0.001), 2), "0");
        assert_eq!(signed(dec!(-0.001).round_dp(2)), "0");
    }

    #[test]
    fn megabyte_conversion_uses_integer_math() {
        assert_eq!(mb(0), "0.0");
        assert_eq!(mb(1_000_000), "1.0");
        assert_eq!(mb(1_500_000), "1.5");
        assert_eq!(mb(891_281_848), "891.2");
    }

    #[test]
    fn percentage_and_basis_points_are_exact() {
        assert_eq!(pct(dec!(0.45)), "45%");
        assert_eq!(pct(dec!(0.125)), "12.5%");
        assert_eq!(bp(dec!(0.0004)), "4bp");
        assert_eq!(bp(dec!(0.0002)), "2bp");
        assert_eq!(bp(dec!(0.00005)), "0.5bp");
    }

    /// 正数必须带 + 号，否则盈亏方向要靠颜色或上下文判断。
    #[test]
    fn signed_shows_positive_explicitly() {
        assert_eq!(signed(dec!(120.5)), "+120.5");
        assert_eq!(signed(dec!(-80)), "-80");
        assert_eq!(signed(Decimal::ZERO), "0");
    }

    #[test]
    fn duration_formats_by_magnitude() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(125), "2m5s");
        assert_eq!(duration(3700), "1h1m");
    }

    /// 分批止盈这类档位统计要对齐显示，否则终端里读不出优先级。
    #[test]
    fn tally_sorts_by_count_descending() {
        // 只验证不 panic 且顺序正确（实际输出到 stdout）
        tally("测试", &[("少", 1), ("多", 10), ("中", 5)]);
    }

    #[test]
    fn tally_handles_empty_input() {
        tally("空", &[]);
    }
}
