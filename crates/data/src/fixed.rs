//! 定点数编解码。
//!
//! # 为什么落盘用定点整数而不是字符串或浮点
//!
//! 三个选项的取舍：
//!
//! | 表示 | 精度 | 体积 | 风险 |
//! | --- | --- | --- | --- |
//! | `f64` | 有误差 | 8 B | 止盈目标是 bp 级时误差足以翻转结论。**禁止** |
//! | 字符串 | 精确 | 约 8-20 B | 体积大，且列剪枝后还要重新解析 |
//! | `i64` 定点 | 精确 | 8 B | 需要约定缩放因子，但一旦约定就无误差 |
//!
//! 币安的价格与数量最多 8 位小数（`pricePrecision`/`quantityPrecision` ≤ 8），
//! 所以按 `1e8` 缩放能把任意合法的 `Decimal` **无损**映射到 `i64`：只要小数
//! 位数不超过 8 位，乘 `1e8` 后必为整数。
//!
//! 这个表示是落盘格式，不是运行期类型——读回后立刻还原成 `Decimal`，
//! 业务代码永远看不到 `i64`。
//!
//! 资金费率的缩放因子不同（`1e18`）：它是 0.00004296 这种小数值，用 `1e8`
//! 会把有效数字压到 4 位，而费率需要更多精度才能正确累加到权益曲线。

use rust_decimal::Decimal;

/// 价格与数量的缩放因子：1e8。
pub const PRICE_SCALE: i64 = 100_000_000;

/// 费率的缩放因子：1e18。
pub const RATE_SCALE: i64 = 1_000_000_000_000_000_000;

/// 币安价格/数量的最大小数位数。超过此值的输入视为异常。
pub const MAX_DECIMALS: u32 = 8;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FixedError {
    #[error("数值 {value} 的小数位数 {decimals} 超过支持的上限 {max}")]
    TooManyDecimals {
        value: String,
        decimals: u32,
        max: u32,
    },
    #[error("数值 {value} 乘以缩放因子后超出 i64 范围")]
    Overflow { value: String },
    #[error("无法解析为数值：{0}")]
    Parse(String),
}

/// 把 `Decimal` 编码为定点 `i64`（价格/数量，1e8）。
///
/// 小数位超过 8 位时报错而不是截断——静默丢精度正是要避免的事。
pub fn encode_price(value: Decimal) -> Result<i64, FixedError> {
    encode_scaled(value, PRICE_SCALE, MAX_DECIMALS)
}

/// 把定点 `i64` 解码回 `Decimal`。
pub fn decode_price(raw: i64) -> Decimal {
    decode_scaled(raw, PRICE_SCALE)
}

/// 把 `Decimal` 编码为定点 `i64`（费率，1e18）。
///
/// 费率不需要 `MAX_DECIMALS` 限制——它本来就该有更多有效数字。
pub fn encode_rate(value: Decimal) -> Result<i64, FixedError> {
    encode_scaled(value, RATE_SCALE, 18)
}

pub fn decode_rate(raw: i64) -> Decimal {
    decode_scaled(raw, RATE_SCALE)
}

fn encode_scaled(value: Decimal, scale: i64, max_decimals: u32) -> Result<i64, FixedError> {
    let decimals = value.scale();
    if decimals > max_decimals {
        return Err(FixedError::TooManyDecimals {
            value: value.to_string(),
            decimals,
            max: max_decimals,
        });
    }

    // 用 Decimal 自身做缩放再转 i64，避免先转 f64 引入误差。
    let scaled = value * Decimal::from(scale);
    // 缩放到 8 位以内后必然没有小数部分；截断是安全的且能容忍额外尾零。
    let truncated = scaled.trunc();
    if truncated != scaled {
        // 理论上不可达（上面已限制 scale），保留以防 Decimal 行为变化。
        return Err(FixedError::TooManyDecimals {
            value: value.to_string(),
            decimals,
            max: max_decimals,
        });
    }
    truncated
        .to_string()
        .parse::<i64>()
        .map_err(|_| FixedError::Overflow {
            value: value.to_string(),
        })
}

fn decode_scaled(raw: i64, scale: i64) -> Decimal {
    Decimal::from(raw) / Decimal::from(scale)
}

/// 从 CSV 字段解析 `Decimal`。空字符串按 0 处理（币安部分字段会留空）。
pub fn parse_decimal(s: &str) -> Result<Decimal, FixedError> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(Decimal::ZERO);
    }
    t.parse::<Decimal>()
        .map_err(|_| FixedError::Parse(t.to_string()))
}

/// 从 CSV 字段解析定点价格。便捷函数，合并解析与编码。
pub fn parse_price(s: &str) -> Result<i64, FixedError> {
    encode_price(parse_decimal(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn price_round_trips_exactly() {
        for v in [
            dec!(1860.24),
            dec!(0.00000001),
            dec!(2466.04),
            dec!(100000.5),
        ] {
            let enc = encode_price(v).unwrap();
            assert_eq!(decode_price(enc), v, "往返必须无损：{v}");
        }
    }

    #[test]
    fn max_precision_is_preserved() {
        // 币安最多 8 位小数，正好落在定点精度上
        let v = dec!(12345.12345678);
        assert_eq!(decode_price(encode_price(v).unwrap()), v);
    }

    #[test]
    fn too_many_decimals_is_an_error_not_a_truncation() {
        // 9 位小数超出支持范围，必须报错而不是悄悄截断
        let v = dec!(1.123456789);
        assert!(matches!(
            encode_price(v).unwrap_err(),
            FixedError::TooManyDecimals { .. }
        ));
    }

    /// 这一条是浮点与定点的分界：0.1 + 0.2 在 f64 下不等于 0.3，
    /// 在定点表示下必须精确等于。
    #[test]
    fn fixed_point_avoids_float_rounding_traps() {
        let a = parse_price("0.1").unwrap();
        let b = parse_price("0.2").unwrap();
        let c = parse_price("0.3").unwrap();
        assert_eq!(a + b, c, "定点加法必须精确");
    }

    #[test]
    fn rate_keeps_small_value_precision() {
        // 资金费典型量级 0.00004296，用价格缩放因子会丢掉有效数字
        let r = dec!(0.00004296);
        let enc = encode_rate(r).unwrap();
        assert_eq!(decode_rate(enc), r);
    }

    #[test]
    fn empty_csv_field_is_zero() {
        assert_eq!(parse_decimal("").unwrap(), Decimal::ZERO);
        assert_eq!(parse_decimal("  ").unwrap(), Decimal::ZERO);
    }

    #[test]
    fn unparseable_field_is_an_error() {
        assert!(matches!(
            parse_decimal("not-a-number").unwrap_err(),
            FixedError::Parse(_)
        ));
    }

    #[test]
    fn negative_values_round_trip() {
        // 资金费可以为负
        let v = dec!(-0.00012345);
        assert_eq!(decode_rate(encode_rate(v).unwrap()), v);
    }
}
