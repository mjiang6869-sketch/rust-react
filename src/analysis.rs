use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::model::Candle;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PivotKind {
    High,
    Low,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrendLineKind {
    Resistance,
    Support,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrendDirection {
    Up,
    Down,
    Sideways,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct Pivot {
    pub time: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    pub kind: PivotKind,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrendLine {
    pub kind: TrendLineKind,
    pub start: Pivot,
    pub end: Pivot,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrendAnalysis {
    pub candles: Vec<Candle>,
    pub pivots: Vec<Pivot>,
    pub trend_lines: Vec<TrendLine>,
    pub direction: TrendDirection,
    pub method: &'static str,
}

pub fn analyze(candles: &[Candle]) -> TrendAnalysis {
    let candles: Vec<Candle> = candles.iter().filter(|c| c.closed).cloned().collect();
    let mut pivots = Vec::new();
    for window in candles.windows(5) {
        let center = &window[2];
        let is_high = window[..2].iter().all(|c| center.high > c.high)
            && window[3..].iter().all(|c| center.high >= c.high);
        let is_low = window[..2].iter().all(|c| center.low < c.low)
            && window[3..].iter().all(|c| center.low <= c.low);
        if is_high {
            pivots.push(Pivot {
                time: center.open_time,
                price: center.high,
                kind: PivotKind::High,
            });
        }
        if is_low {
            pivots.push(Pivot {
                time: center.open_time,
                price: center.low,
                kind: PivotKind::Low,
            });
        }
    }
    let highs: Vec<Pivot> = pivots
        .iter()
        .filter(|pivot| pivot.kind == PivotKind::High)
        .cloned()
        .collect();
    let lows: Vec<Pivot> = pivots
        .iter()
        .filter(|pivot| pivot.kind == PivotKind::Low)
        .cloned()
        .collect();
    let mut trend_lines = Vec::new();
    if highs.len() >= 2 {
        trend_lines.push(TrendLine {
            kind: TrendLineKind::Resistance,
            start: highs[highs.len() - 2].clone(),
            end: highs[highs.len() - 1].clone(),
        });
    }
    if lows.len() >= 2 {
        trend_lines.push(TrendLine {
            kind: TrendLineKind::Support,
            start: lows[lows.len() - 2].clone(),
            end: lows[lows.len() - 1].clone(),
        });
    }
    let direction = candles
        .first()
        .zip(candles.last())
        .map(|(first, last)| {
            let change = last.close - first.close;
            let threshold = first.close.abs() * Decimal::new(1, 3);
            if change > threshold {
                TrendDirection::Up
            } else if change < -threshold {
                TrendDirection::Down
            } else {
                TrendDirection::Sideways
            }
        })
        .unwrap_or(TrendDirection::Unknown);
    TrendAnalysis {
        candles,
        pivots,
        trend_lines,
        direction,
        method: "five_bar_pivot_v1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn candle(time: DateTime<Utc>, high: i64, low: i64, close: i64) -> Candle {
        Candle {
            open_time: time,
            open: Decimal::from(close),
            high: Decimal::from(high),
            low: Decimal::from(low),
            close: Decimal::from(close),
            closed: true,
        }
    }

    #[test]
    fn detects_pivots_and_direction_without_float_math() {
        let start = Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0).unwrap();
        let candles = vec![
            candle(start, 100, 98, 99),
            candle(start + Duration::minutes(1), 101, 99, 100),
            candle(start + Duration::minutes(2), 105, 100, 104),
            candle(start + Duration::minutes(3), 102, 99, 100),
            candle(start + Duration::minutes(4), 101, 98, 99),
            candle(start + Duration::minutes(5), 103, 100, 102),
            candle(start + Duration::minutes(6), 104, 101, 103),
        ];
        let result = analyze(&candles);
        assert!(
            result
                .pivots
                .iter()
                .any(|pivot| pivot.kind == PivotKind::High)
        );
        assert_eq!(result.direction, TrendDirection::Up);
    }
}
