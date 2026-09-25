//! 交易数值的新类型。
//!
//! 全仓库禁止用 `f32`/`f64` 承载价格、数量、费率或保证金。见 `clippy.toml`
//! 的 `disallowed-types`，这是编译期强制而非约定。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 价格。序列化为字符串，避免 JSON 浮点往返丢精度。
///
/// 用新类型而非裸 `Decimal` 的价值：函数签名能区分"这是价格"和"这是数量"，
/// 让 `submit(price, qty)` 这类参数顺序错误在编译期就暴露。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Price(#[serde(with = "rust_decimal::serde::str")] pub Decimal);

/// 数量。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Qty(#[serde(with = "rust_decimal::serde::str")] pub Decimal);

impl Price {
    pub const fn new(d: Decimal) -> Self {
        Self(d)
    }
    pub const fn get(self) -> Decimal {
        self.0
    }
    pub fn is_positive(self) -> bool {
        self.0 > Decimal::ZERO
    }
}

impl Qty {
    pub const ZERO: Self = Self(Decimal::ZERO);

    pub const fn new(d: Decimal) -> Self {
        Self(d)
    }
    pub const fn get(self) -> Decimal {
        self.0
    }
    pub fn is_positive(self) -> bool {
        self.0 > Decimal::ZERO
    }
    pub fn is_zero(self) -> bool {
        self.0.is_zero()
    }
}

impl std::fmt::Display for Price {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for Qty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::ops::Add for Qty {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl std::ops::Sub for Qty {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}
