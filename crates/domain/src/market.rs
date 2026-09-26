//! 行情事件与时间。
//!
//! 所有模式（回测 / 模拟盘 / 实盘）的**唯一输入类型**。策略、撮合、保护单
//! 规划器都只认这一组类型，所以同一份策略代码能在三种模式下产生完全相同的
//! 行为——这是本项目的核心架构承诺。

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::money::{Price, Qty};

/// 一根 K 线。
///
/// `closed` 区分"正在形成"与"已收盘"。策略**只能**使用 `closed == true`
/// 的 K 线做决策——用未收盘 K 线会让回测偷看未来。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(with = "rust_decimal::serde::str")]
    pub volume: Decimal,
    pub closed: bool,
}

impl Candle {
    /// K 线结束时刻（1 分钟周期）。
    pub fn close_time(&self) -> DateTime<Utc> {
        self.open_time + chrono::Duration::minutes(1)
    }
}

/// 一笔聚合成交。
///
/// `is_buyer_maker` 是判断主动方向的唯一依据：`true` 表示买方是挂单方，
/// 也就是**卖方主动**吃掉了买单。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggTrade {
    pub trade_id: u64,
    pub price: Price,
    pub quantity: Qty,
    pub is_buyer_maker: bool,
    /// 交易所上报的成交时刻（毫秒精度）。同一毫秒可能有多笔，但
    /// `trade_id` 单调递增，所以顺序仍可确定重放。
    pub at: DateTime<Utc>,
}

impl AggTrade {
    /// 主动方向：`is_buyer_maker == true` 时为卖，否则为买。
    pub fn aggressor_is_sell(&self) -> bool {
        self.is_buyer_maker
    }
}

/// 盘口快照（最优买卖价 + 可选深度）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookSnapshot {
    #[serde(with = "rust_decimal::serde::str")]
    pub bid: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub ask: Decimal,
    /// 买盘各档（价, 量），从最优价往下。
    #[serde(default)]
    pub bids: Vec<(Decimal, Decimal)>,
    /// 卖盘各档，从最优价往上。
    #[serde(default)]
    pub asks: Vec<(Decimal, Decimal)>,
    pub at: DateTime<Utc>,
}

impl BookSnapshot {
    pub fn mid(&self) -> Decimal {
        (self.bid + self.ask) / Decimal::TWO
    }
}

/// 喂给策略与撮合引擎的行情事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketEvent {
    /// 已收盘或正在形成的 K 线。`at()` 对已收盘的 K 线返回收盘时刻，
    /// 而非开盘时刻——见 `at()` 的文档。
    Kline(Candle),
    /// 逐笔成交。M1 成交模型的主要输入。
    AggTrade(AggTrade),
    /// 盘口快照。
    Book(BookSnapshot),
    /// 标记价。用于估算强平距离。
    MarkPrice { price: Price, at: DateTime<Utc> },
    /// 时钟推进。驱动定时取消与移动止损，让所有时间相关行为确定可重放。
    Clock(DateTime<Utc>),
}

impl MarketEvent {
    /// 事件时刻。
    ///
    /// K 线的时刻取决于是否已收盘：已收盘的 K 线在**收盘时刻**才真正可知
    /// （用 `close_time()`），若按开盘时刻当"现在"，策略在一分钟开头就能
    /// 看到整分钟的收盘数据，等同于偷看未来；模拟盘里挂单成交判定也会被
    /// 记早一分钟。仍在形成中的 K 线（`closed == false`）本就代表"当前
    /// 未完成的一分钟"，继续用开盘时刻。
    pub fn at(&self) -> DateTime<Utc> {
        match self {
            MarketEvent::Kline(c) if c.closed => c.close_time(),
            MarketEvent::Kline(c) => c.open_time,
            MarketEvent::AggTrade(t) => t.at,
            MarketEvent::Book(b) => b.at,
            MarketEvent::MarkPrice { at, .. } => *at,
            MarketEvent::Clock(t) => *t,
        }
    }

    /// 该事件是否携带可用于成交判定的成交信息。
    ///
    /// M1 模型只信任 `AggTrade`——K 线的 wick 触价**不足以**判定成交，
    /// 那正是 M0 会骗人的地方。
    pub fn is_trade(&self) -> bool {
        matches!(self, MarketEvent::AggTrade(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn trade(id: u64, px: Decimal, buyer_maker: bool) -> AggTrade {
        AggTrade {
            trade_id: id,
            price: Price::new(px),
            quantity: Qty::new(dec!(1)),
            is_buyer_maker: buyer_maker,
            at: Utc::now(),
        }
    }

    /// 主动方向的判定是 markout 与逆向选择分析的基础，必须锁住。
    #[test]
    fn aggressor_direction_follows_buyer_maker_flag() {
        // is_buyer_maker = true -> 买方挂单 -> 卖方主动
        assert!(trade(1, dec!(100), true).aggressor_is_sell());
        // is_buyer_maker = false -> 卖方挂单 -> 买方主动
        assert!(!trade(2, dec!(100), false).aggressor_is_sell());
    }

    #[test]
    fn candle_close_time_is_one_minute_after_open() {
        let c = Candle {
            open_time: Utc::now(),
            open: dec!(1),
            high: dec!(2),
            low: dec!(1),
            close: dec!(2),
            volume: dec!(1),
            closed: true,
        };
        assert_eq!(c.close_time() - c.open_time, chrono::Duration::minutes(1));
    }

    /// 1m K 线天然是 1440 根/天，这是台账校验行数的依据。
    #[test]
    fn one_day_contains_exactly_1440_minutes() {
        let day = chrono::Duration::days(1).num_minutes();
        assert_eq!(day, 1440);
    }

    /// M1 只信任逐笔成交，K 线的 wick 不算成交依据。
    #[test]
    fn only_agg_trades_count_as_trade_evidence() {
        let candle = MarketEvent::Kline(Candle {
            open_time: Utc::now(),
            open: dec!(100),
            high: dec!(110),
            low: dec!(90),
            close: dec!(105),
            volume: dec!(10),
            closed: true,
        });
        assert!(!candle.is_trade(), "K 线触价不能作为成交依据");

        assert!(MarketEvent::AggTrade(trade(1, dec!(100), false)).is_trade());
    }

    /// 已收盘 K 线只有在收盘时刻才真正可知，`at()` 必须返回收盘时刻，
    /// 否则策略/撮合会在一分钟开头就看到整分钟的收盘数据。
    #[test]
    fn closed_kline_at_is_close_time() {
        let open_time = Utc::now();
        let candle = MarketEvent::Kline(Candle {
            open_time,
            open: dec!(100),
            high: dec!(110),
            low: dec!(90),
            close: dec!(105),
            volume: dec!(10),
            closed: true,
        });
        assert_eq!(candle.at(), open_time + chrono::Duration::minutes(1));
    }

    /// 未收盘 K 线代表"当前未完成的一分钟"，`at()` 仍用开盘时刻。
    #[test]
    fn unclosed_kline_at_is_open_time() {
        let open_time = Utc::now();
        let candle = MarketEvent::Kline(Candle {
            open_time,
            open: dec!(100),
            high: dec!(110),
            low: dec!(90),
            close: dec!(105),
            volume: dec!(10),
            closed: false,
        });
        assert_eq!(candle.at(), open_time);
    }

    #[test]
    fn book_mid_is_average_of_bid_and_ask() {
        let b = BookSnapshot {
            bid: dec!(100),
            ask: dec!(101),
            bids: vec![],
            asks: vec![],
            at: Utc::now(),
        };
        assert_eq!(b.mid(), dec!(100.5));
    }
}
