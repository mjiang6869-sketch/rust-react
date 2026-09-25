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
pub struct ChanStroke {
    pub start: Pivot,
    pub end: Pivot,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChanSegment {
    pub start: Pivot,
    pub end: Pivot,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChanCenter {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    pub low: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub high: Decimal,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrendAnalysis {
    pub candles: Vec<Candle>,
    pub pivots: Vec<Pivot>,
    pub trend_lines: Vec<TrendLine>,
    pub chan_strokes: Vec<ChanStroke>,
    pub chan_segments: Vec<ChanSegment>,
    pub chan_centers: Vec<ChanCenter>,
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
    let alternating = normalize_pivots(&pivots);
    let chan_strokes: Vec<ChanStroke> = alternating
        .windows(2)
        .map(|pair| ChanStroke {
            start: pair[0].clone(),
            end: pair[1].clone(),
        })
        .collect();
    let chan_segments: Vec<ChanSegment> = alternating
        .windows(3)
        .map(|triple| ChanSegment {
            start: triple[0].clone(),
            end: triple[2].clone(),
        })
        .collect();
    let chan_centers = chan_centers(&chan_strokes);
    TrendAnalysis {
        candles,
        pivots,
        trend_lines,
        chan_strokes,
        chan_segments,
        chan_centers,
        direction,
        method: "five_bar_pivot_chan_v1",
    }
}

fn normalize_pivots(pivots: &[Pivot]) -> Vec<Pivot> {
    let mut result: Vec<Pivot> = Vec::new();
    for pivot in pivots {
        if let Some(previous) = result.last_mut()
            && previous.kind == pivot.kind
        {
            let replace = match pivot.kind {
                PivotKind::High => pivot.price > previous.price,
                PivotKind::Low => pivot.price < previous.price,
            };
            if replace {
                *previous = pivot.clone();
            }
        } else {
            result.push(pivot.clone());
        }
    }
    result
}

fn chan_centers(strokes: &[ChanStroke]) -> Vec<ChanCenter> {
    strokes
        .windows(3)
        .filter_map(|window| {
            let lows = [
                window[0].start.price.min(window[0].end.price),
                window[1].start.price.min(window[1].end.price),
                window[2].start.price.min(window[2].end.price),
            ];
            let highs = [
                window[0].start.price.max(window[0].end.price),
                window[1].start.price.max(window[1].end.price),
                window[2].start.price.max(window[2].end.price),
            ];
            let low = lows.into_iter().max()?;
            let high = highs.into_iter().min()?;
            (low < high).then_some(ChanCenter {
                start_time: window[0].start.time,
                end_time: window[2].end.time,
                low,
                high,
            })
        })
        .collect()
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
        assert!(!result.chan_strokes.is_empty());
    }
}
