//! 成交带：一段按时间排序的逐笔成交。
//!
//! # 为什么需要它
//!
//! M1 成交模型要求"有成交发生在我们的价位上"才算成交。这个判定必须只看
//! **挂单时刻之后**的成交——否则会用挂单前的成交给自己"成交"，那是偷看
//! 未来的一种形式。
//!
//! # 确定性
//!
//! 成交带按 `(时间, trade_id)` 排序。币安的 `agg_trade_id` 单调递增，
//! 所以即使同一毫秒有多笔（实测最密集的一毫秒有 218 笔），顺序也是确定的。
//! 这让回测完全可重放：相同输入必得相同输出。

use chrono::{DateTime, Utc};
use domain::{AggTrade, Price, Qty};

/// 一笔成交（成交带的元素）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trade {
    pub trade_id: u64,
    pub price: Price,
    pub quantity: Qty,
    pub is_buyer_maker: bool,
    pub at: DateTime<Utc>,
}

impl From<AggTrade> for Trade {
    fn from(t: AggTrade) -> Self {
        Self {
            trade_id: t.trade_id,
            price: t.price,
            quantity: t.quantity,
            is_buyer_maker: t.is_buyer_maker,
            at: t.at,
        }
    }
}

/// 一段逐笔成交，按 `(时间, trade_id)` 升序。
#[derive(Clone, Debug, Default)]
pub struct TradeTape {
    trades: Vec<Trade>,
}

impl TradeTape {
    /// 从已排序的成交构造。会做一次排序以保证前提成立。
    pub fn from_trades(mut trades: Vec<Trade>) -> Self {
        trades.sort_by(|a, b| a.at.cmp(&b.at).then(a.trade_id.cmp(&b.trade_id)));
        Self { trades }
    }

    pub fn is_empty(&self) -> bool {
        self.trades.is_empty()
    }

    pub fn len(&self) -> usize {
        self.trades.len()
    }

    /// 全部成交。
    pub fn trades(&self) -> &[Trade] {
        &self.trades
    }

    /// 时间区间 `[from, to)` 内的成交切片。
    ///
    /// 用二分查找定位起点，避免每个 tick 都从头扫描整条带——回测里这个函数
    /// 每个行情事件都会被调用。
    pub fn window(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> &[Trade] {
        let start = self.trades.partition_point(|t| t.at < from);
        let end = self.trades.partition_point(|t| t.at < to);
        &self.trades[start..end]
    }

    /// 某时刻之后的全部成交。
    pub fn after(&self, from: DateTime<Utc>) -> &[Trade] {
        let start = self.trades.partition_point(|t| t.at < from);
        &self.trades[start..]
    }

    /// 最优买卖价的估计（用成交价近似）。
    ///
    /// 注意：`bookTicker` 归档已于 2024-04 停更，所以历史回测**没有**真实
    /// 盘口。这里用最近一段成交的高低点近似，仅供 markout 参考价的粗略估计，
    /// 不能当作真实买卖价差使用。
    pub fn approx_bid_ask(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Option<(Price, Price)> {
        let w = self.window(from, to);
        if w.is_empty() {
            return None;
        }
        let mut lo = w[0].price;
        let mut hi = w[0].price;
        for t in w.iter().skip(1) {
            if t.price < lo {
                lo = t.price;
            }
            if t.price > hi {
                hi = t.price;
            }
        }
        Some((lo, hi))
    }

    /// 某时刻之后第一笔成交的价格。用于 markout 计算。
    pub fn first_price_after(&self, t: DateTime<Utc>) -> Option<Price> {
        let start = self.trades.partition_point(|x| x.at < t);
        self.trades.get(start).map(|x| x.price)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn t(id: u64, at_ms: i64, px: Decimal) -> Trade {
        Trade {
            trade_id: id,
            price: Price::new(px),
            quantity: Qty::new(dec!(1)),
            is_buyer_maker: false,
            at: chrono::DateTime::from_timestamp_millis(at_ms).unwrap(),
        }
    }

    const T0: i64 = 1_785_542_400_000;

    #[test]
    fn from_trades_sorts_by_time_then_id() {
        let tape = TradeTape::from_trades(vec![
            t(3, T0 + 20, dec!(100)),
            t(1, T0 + 10, dec!(101)),
            t(2, T0 + 10, dec!(102)),
        ]);
        let ids: Vec<u64> = tape.trades().iter().map(|x| x.trade_id).collect();
        assert_eq!(ids, vec![1, 2, 3], "先按时间，同时间按 id");
    }

    /// 同一毫秒内的多笔必须靠 trade_id 保持确定顺序——这是可重放的前提。
    #[test]
    fn same_millisecond_order_is_deterministic() {
        let mk = || {
            TradeTape::from_trades(vec![
                t(500, T0 + 1, dec!(100)),
                t(498, T0 + 1, dec!(101)),
                t(499, T0 + 1, dec!(102)),
            ])
        };
        let a: Vec<u64> = mk().trades().iter().map(|x| x.trade_id).collect();
        let b: Vec<u64> = mk().trades().iter().map(|x| x.trade_id).collect();
        assert_eq!(a, b);
        assert_eq!(a, vec![498, 499, 500]);
    }

    #[test]
    fn window_is_half_open() {
        let tape = TradeTape::from_trades(vec![
            t(1, T0, dec!(100)),
            t(2, T0 + 10, dec!(101)),
            t(3, T0 + 20, dec!(102)),
        ]);
        let from = chrono::DateTime::from_timestamp_millis(T0 + 10).unwrap();
        let to = chrono::DateTime::from_timestamp_millis(T0 + 20).unwrap();
        let w = tape.window(from, to);
        assert_eq!(w.len(), 1, "含起点不含终点");
        assert_eq!(w[0].trade_id, 2);
    }

    /// `after` 是 M1 的核心查询：只看挂单之后的成交。
    #[test]
    fn after_excludes_trades_before_placement() {
        let tape = TradeTape::from_trades(vec![
            t(1, T0, dec!(100)),
            t(2, T0 + 10, dec!(101)),
            t(3, T0 + 20, dec!(102)),
        ]);
        let placed = chrono::DateTime::from_timestamp_millis(T0 + 10).unwrap();
        let after = tape.after(placed);
        assert_eq!(after.len(), 2, "挂单当时的成交算在内");
        assert_eq!(after[0].trade_id, 2);
    }

    /// 这个测试锁住"不能用挂单前的成交给自己成交"。
    #[test]
    fn placement_time_boundary_prevents_lookahead() {
        let tape = TradeTape::from_trades(vec![
            t(1, T0, dec!(3190)), // 挂单前就跌到过我们的买价
            t(2, T0 + 10, dec!(3205)),
        ]);
        let placed = chrono::DateTime::from_timestamp_millis(T0 + 5).unwrap();
        let after = tape.after(placed);
        // 挂单后只有 3205，我们的 3190 买价没有被触及
        assert!(after.iter().all(|x| x.price.get() > dec!(3195)));
    }

    #[test]
    fn empty_tape_is_handled() {
        let tape = TradeTape::default();
        assert!(tape.is_empty());
        let from = chrono::DateTime::from_timestamp_millis(T0).unwrap();
        assert!(tape.window(from, from).is_empty());
        assert!(tape.first_price_after(from).is_none());
        assert!(tape.approx_bid_ask(from, from).is_none());
    }

    #[test]
    fn first_price_after_finds_next_trade() {
        let tape = TradeTape::from_trades(vec![t(1, T0, dec!(100)), t(2, T0 + 10, dec!(105))]);
        let t_after = chrono::DateTime::from_timestamp_millis(T0 + 5).unwrap();
        assert_eq!(tape.first_price_after(t_after).unwrap().get(), dec!(105));
    }

    #[test]
    fn approx_bid_ask_uses_observed_high_low() {
        let tape = TradeTape::from_trades(vec![
            t(1, T0, dec!(100)),
            t(2, T0 + 10, dec!(102)),
            t(3, T0 + 20, dec!(98)),
        ]);
        let from = chrono::DateTime::from_timestamp_millis(T0).unwrap();
        let to = chrono::DateTime::from_timestamp_millis(T0 + 30).unwrap();
        let (lo, hi) = tape.approx_bid_ask(from, to).unwrap();
        assert_eq!(lo.get(), dec!(98));
        assert_eq!(hi.get(), dec!(102));
    }

    /// 大量成交时二分查找必须正确（覆盖分区边界）。
    #[test]
    fn window_on_large_tape_partitions_correctly() {
        let trades: Vec<Trade> = (0..1000)
            .map(|i| t(i as u64, T0 + i * 10, dec!(100)))
            .collect();
        let tape = TradeTape::from_trades(trades);
        let from = chrono::DateTime::from_timestamp_millis(T0 + 5000).unwrap();
        let to = chrono::DateTime::from_timestamp_millis(T0 + 5100).unwrap();
        let w = tape.window(from, to);
        assert_eq!(w.len(), 10, "500..509 共 10 笔");
        assert_eq!(w[0].trade_id, 500);
        assert_eq!(w[9].trade_id, 509);
    }
}
