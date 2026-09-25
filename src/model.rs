use chrono::{DateTime, Duration, Timelike, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candle {
    pub open_time: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub open: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub high: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub low: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub close: Decimal,
    pub closed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StrategyKind {
    #[default]
    Retest,
    ChanCenter,
}

impl Side {
    pub fn label(self) -> &'static str {
        match self {
            Self::Buy => "做多",
            Self::Sell => "做空",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Signal {
    #[serde(default)]
    pub strategy: StrategyKind,
    pub side: Side,
    #[serde(with = "rust_decimal::serde::str")]
    pub entry_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub stop_price: Decimal,
    pub confirmed_at: DateTime<Utc>,
    pub range_start: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub range_low: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub range_high: Decimal,
}

impl Signal {
    pub fn expires_at(&self) -> DateTime<Utc> {
        (self.confirmed_at + Duration::minutes(2)).min(self.range_start + Duration::minutes(15))
    }

    pub fn id(&self) -> String {
        format!(
            "mm-entry:retest:{}:{:?}",
            self.confirmed_at.timestamp(),
            self.side
        )
    }
}

pub fn quarter_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now.with_minute(now.minute() / 15 * 15)
        .expect("valid quarter minute")
        .with_second(0)
        .expect("valid second")
        .with_nanosecond(0)
        .expect("valid nanosecond")
}

pub fn quantize_down(value: Decimal, step: Decimal) -> Decimal {
    (value / step).floor() * step
}

pub fn quantize_up(value: Decimal, step: Decimal) -> Decimal {
    (value / step).ceil() * step
}
