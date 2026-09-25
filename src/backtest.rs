use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::model::{Candle, Side, Signal, quantize_down};
use crate::signal::{find_reversal_signal, invalid_reason, risk_reason};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FillModel {
    CandleRangeTouch,
    TopOfBook,
}

#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct OrderBookSnapshot {
    pub open_time: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub bid_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub bid_quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub ask_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub ask_quantity: Decimal,
}

#[derive(Clone, Debug)]
pub struct BacktestConfig {
    pub initial_equity: Decimal,
    pub margin_pct: Decimal,
    pub leverage: Decimal,
    pub stop_pct: Decimal,
    pub take_profit_pct: Decimal,
    pub maker_fee_pct: Decimal,
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub min_qty: Decimal,
    pub min_notional: Decimal,
    pub fill_model: FillModel,
    pub order_book: BTreeMap<DateTime<Utc>, OrderBookSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
}

#[derive(Clone, Debug, Serialize)]
pub struct BacktestTrade {
    pub side: Side,
    pub entry_time: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub entry_price: Decimal,
    pub exit_time: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub exit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
    pub exit_reason: ExitReason,
    #[serde(with = "rust_decimal::serde::str")]
    pub pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fees: Decimal,
}

#[derive(Clone, Debug, Serialize)]
pub struct EquityPoint {
    pub time: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub equity: Decimal,
}

#[derive(Clone, Debug, Serialize)]
pub struct BacktestReport {
    pub fill_model: FillModel,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    #[serde(with = "rust_decimal::serde::str")]
    pub initial_equity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub final_equity: Decimal,
    pub trades: Vec<BacktestTrade>,
    pub equity_curve: Vec<EquityPoint>,
    pub data_gaps: Vec<DateTime<Utc>>,
    pub pending_expiries: u32,
}

#[derive(Clone, Debug)]
struct PendingEntry {
    signal: Signal,
    quantity: Decimal,
}

#[derive(Clone, Debug)]
struct OpenPosition {
    side: Side,
    quantity: Decimal,
    entry_price: Decimal,
    stop_price: Decimal,
    target_price: Decimal,
    opened_at: DateTime<Utc>,
    stop_triggered: bool,
}

pub fn run(candles: &[Candle], config: &BacktestConfig) -> BacktestReport {
    let mut history = BTreeMap::new();
    let mut pending: Option<PendingEntry> = None;
    let mut position: Option<OpenPosition> = None;
    let mut used_signal_at = None;
    let mut equity = config.initial_equity;
    let mut trades = Vec::new();
    let mut equity_curve = Vec::new();
    let mut data_gaps = Vec::new();
    let mut pending_expiries = 0;

    for (index, candle) in candles.iter().enumerate() {
        if !candle.closed {
            continue;
        }
        if let Some(previous) = index.checked_sub(1).and_then(|i| candles.get(i))
            && candle.open_time - previous.open_time != Duration::minutes(1)
        {
            data_gaps.push(previous.open_time + Duration::minutes(1));
        }
        history.insert(candle.open_time, candle.clone());
        let now = candle.open_time + Duration::minutes(1);

        if let Some(open) = position.as_mut()
            && let Some((exit_price, exit_reason)) = exit_fill(open, candle)
        {
            let pnl = match open.side {
                Side::Buy => (exit_price - open.entry_price) * open.quantity,
                Side::Sell => (open.entry_price - exit_price) * open.quantity,
            };
            let fees = (open.entry_price + exit_price) * open.quantity * config.maker_fee_pct
                / Decimal::from(100);
            equity += pnl - fees;
            trades.push(BacktestTrade {
                side: open.side,
                entry_time: open.opened_at,
                entry_price: open.entry_price,
                exit_time: now,
                exit_price,
                quantity: open.quantity,
                exit_reason,
                pnl,
                fees,
            });
            position = None;
        }

        if position.is_none()
            && let Some(entry) = pending.take()
        {
            if now >= entry.signal.expires_at()
                || invalid_reason(&entry.signal, Some(candle), &history, now).is_some()
            {
                pending_expiries += 1;
            } else if touches_entry(
                entry.signal.side,
                entry.signal.entry_price,
                entry.quantity,
                candle,
                config,
            ) {
                let ratio = config.take_profit_pct / Decimal::from(100);
                let target_price = match entry.signal.side {
                    Side::Buy => quantize_down(
                        entry.signal.entry_price * (Decimal::ONE + ratio),
                        config.tick_size,
                    ),
                    Side::Sell => quantize_down(
                        entry.signal.entry_price * (Decimal::ONE - ratio),
                        config.tick_size,
                    ),
                };
                position = Some(OpenPosition {
                    side: entry.signal.side,
                    quantity: entry.quantity,
                    entry_price: entry.signal.entry_price,
                    stop_price: entry.signal.stop_price,
                    target_price,
                    opened_at: now,
                    stop_triggered: false,
                });
            }
        }

        if position.is_none()
            && pending.is_none()
            && let Some(signal) = find_reversal_signal(&history, now, config.tick_size)
            && used_signal_at.is_none_or(|used| signal.confirmed_at > used)
        {
            used_signal_at = Some(signal.confirmed_at);
            let valid = invalid_reason(&signal, Some(candle), &history, now).is_none()
                && risk_reason(&signal, config.stop_pct, config.take_profit_pct).is_none();
            if valid {
                let quantity = quantize_down(
                    equity * config.margin_pct * config.leverage
                        / Decimal::from(100)
                        / signal.entry_price,
                    config.step_size,
                );
                if quantity >= config.min_qty
                    && quantity * signal.entry_price >= config.min_notional
                {
                    pending = Some(PendingEntry { signal, quantity });
                }
            }
        }
        equity_curve.push(EquityPoint { time: now, equity });
    }

    BacktestReport {
        fill_model: config.fill_model,
        start_time: candles.first().map(|c| c.open_time),
        end_time: candles.last().map(|c| c.open_time),
        initial_equity: config.initial_equity,
        final_equity: equity,
        trades,
        equity_curve,
        data_gaps,
        pending_expiries,
    }
}

fn touches_entry(
    side: Side,
    price: Decimal,
    quantity: Decimal,
    candle: &Candle,
    config: &BacktestConfig,
) -> bool {
    let candle_touch = match side {
        Side::Buy => candle.low <= price,
        Side::Sell => candle.high >= price,
    };
    if !candle_touch {
        return false;
    }
    match config.fill_model {
        FillModel::CandleRangeTouch => true,
        FillModel::TopOfBook => config
            .order_book
            .get(&candle.open_time)
            .is_some_and(|book| match side {
                Side::Buy => book.bid_price >= price && book.bid_quantity >= quantity,
                Side::Sell => book.ask_price <= price && book.ask_quantity >= quantity,
            }),
    }
}

fn exit_fill(position: &mut OpenPosition, candle: &Candle) -> Option<(Decimal, ExitReason)> {
    position.stop_triggered |= match position.side {
        Side::Buy => candle.low <= position.stop_price,
        Side::Sell => candle.high >= position.stop_price,
    };
    let stop_filled = position.stop_triggered
        && match position.side {
            Side::Buy => candle.high >= position.stop_price,
            Side::Sell => candle.low <= position.stop_price,
        };
    let target_filled = match position.side {
        Side::Buy => candle.high >= position.target_price,
        Side::Sell => candle.low <= position.target_price,
    };
    if stop_filled {
        Some((position.stop_price, ExitReason::StopLoss))
    } else if target_filled {
        Some((position.target_price, ExitReason::TakeProfit))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn config() -> BacktestConfig {
        BacktestConfig {
            initial_equity: Decimal::from(10_000),
            margin_pct: Decimal::from(25),
            leverage: Decimal::ONE,
            stop_pct: Decimal::ONE,
            take_profit_pct: Decimal::new(4, 2),
            maker_fee_pct: Decimal::ZERO,
            tick_size: Decimal::new(1, 2),
            step_size: Decimal::new(1, 3),
            min_qty: Decimal::new(1, 3),
            min_notional: Decimal::from(5),
            fill_model: FillModel::CandleRangeTouch,
            order_book: BTreeMap::new(),
        }
    }

    #[test]
    fn replays_maker_entry_and_take_profit_from_closed_candles() {
        let start = Utc.with_ymd_and_hms(2026, 9, 24, 7, 0, 0).unwrap();
        let mut candles = Vec::new();
        for i in 0..66 {
            let open_time = start + Duration::minutes(i);
            candles.push(Candle {
                open_time,
                open: Decimal::new(9980, 2),
                high: if i == 65 {
                    Decimal::new(10005, 2)
                } else {
                    Decimal::from(100)
                },
                low: if i == 65 {
                    Decimal::new(9950, 2)
                } else {
                    Decimal::new(9940, 2)
                },
                close: if i == 65 {
                    Decimal::new(9998, 2)
                } else {
                    Decimal::new(9980, 2)
                },
                closed: true,
            });
        }
        candles.push(Candle {
            open_time: start + Duration::minutes(66),
            open: Decimal::from(100),
            high: Decimal::from(100),
            low: Decimal::new(9998, 2),
            close: Decimal::new(9998, 2),
            closed: true,
        });
        candles.push(Candle {
            open_time: start + Duration::minutes(67),
            open: Decimal::new(9998, 2),
            high: Decimal::new(9998, 2),
            low: Decimal::new(9995, 2),
            close: Decimal::new(9996, 2),
            closed: true,
        });
        let report = run(&candles, &config());
        assert_eq!(report.trades.len(), 1);
        assert_eq!(report.trades[0].side, Side::Sell);
        assert_eq!(report.trades[0].exit_reason, ExitReason::TakeProfit);
        assert_eq!(report.trades[0].entry_price, Decimal::from(100));
        assert_eq!(report.trades[0].exit_price, Decimal::new(9996, 2));
        assert!(report.final_equity > report.initial_equity);
    }

    #[test]
    fn records_gaps_and_conservative_stop_when_both_prices_are_touched() {
        let start = Utc.with_ymd_and_hms(2026, 9, 24, 7, 0, 0).unwrap();
        let candles = vec![
            Candle {
                open_time: start,
                open: Decimal::ONE,
                high: Decimal::ONE,
                low: Decimal::ONE,
                close: Decimal::ONE,
                closed: true,
            },
            Candle {
                open_time: start + Duration::minutes(2),
                open: Decimal::ONE,
                high: Decimal::ONE,
                low: Decimal::ONE,
                close: Decimal::ONE,
                closed: true,
            },
        ];
        let report = run(&candles, &config());
        assert_eq!(report.data_gaps, vec![start + Duration::minutes(1)]);
        let mut position = OpenPosition {
            side: Side::Buy,
            quantity: Decimal::ONE,
            entry_price: Decimal::from(100),
            stop_price: Decimal::from(99),
            target_price: Decimal::from(101),
            opened_at: start,
            stop_triggered: false,
        };
        let candle = Candle {
            open_time: start,
            open: Decimal::from(100),
            high: Decimal::from(101),
            low: Decimal::from(99),
            close: Decimal::from(100),
            closed: true,
        };
        assert_eq!(
            exit_fill(&mut position, &candle),
            Some((Decimal::from(99), ExitReason::StopLoss))
        );
    }

    #[test]
    fn top_of_book_model_requires_maker_side_liquidity() {
        let start = Utc.with_ymd_and_hms(2026, 9, 24, 7, 0, 0).unwrap();
        let mut candles = Vec::new();
        for i in 0..66 {
            let open_time = start + Duration::minutes(i);
            candles.push(Candle {
                open_time,
                open: Decimal::new(9980, 2),
                high: if i == 65 {
                    Decimal::new(10005, 2)
                } else {
                    Decimal::from(100)
                },
                low: Decimal::new(9940, 2),
                close: if i == 65 {
                    Decimal::new(9998, 2)
                } else {
                    Decimal::new(9980, 2)
                },
                closed: true,
            });
        }
        candles.push(Candle {
            open_time: start + Duration::minutes(66),
            open: Decimal::from(100),
            high: Decimal::from(100),
            low: Decimal::new(9998, 2),
            close: Decimal::new(9998, 2),
            closed: true,
        });
        let entry_time = start + Duration::minutes(66);
        let mut config = config();
        config.fill_model = FillModel::TopOfBook;
        config.order_book.insert(
            entry_time,
            OrderBookSnapshot {
                open_time: entry_time,
                bid_price: Decimal::new(9999, 2),
                bid_quantity: Decimal::from(100),
                ask_price: Decimal::from(100),
                ask_quantity: Decimal::from(100),
            },
        );
        let report = run(&candles, &config);
        assert_eq!(report.fill_model, FillModel::TopOfBook);
        assert_eq!(report.trades.len(), 0);
        let mut blocked = config.clone();
        blocked.order_book.get_mut(&entry_time).unwrap().ask_price = Decimal::new(10001, 2);
        assert!(run(&candles, &blocked).trades.is_empty());
    }
}
