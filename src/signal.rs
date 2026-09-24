use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

use crate::model::{Candle, Side, Signal, quantize_down, quantize_up, quarter_start};

pub fn find_reversal_signal(
    candles: &BTreeMap<DateTime<Utc>, Candle>,
    now: DateTime<Utc>,
    tick_size: Decimal,
) -> Option<Signal> {
    let bucket = quarter_start(now);
    if now - bucket < Duration::minutes(5) || tick_size <= Decimal::ZERO {
        return None;
    }
    let reference: Vec<&Candle> = (1..=60)
        .rev()
        .map(|minute| candles.get(&(bucket - Duration::minutes(minute))))
        .collect::<Option<Vec<_>>>()?;
    let low = reference.iter().map(|c| c.low).min()?;
    let high = reference.iter().map(|c| c.high).max()?;
    if low <= Decimal::ZERO || high <= low {
        return None;
    }

    let mut side = None;
    let mut extreme = Decimal::ZERO;
    let mut signal = None;
    let mut cursor = bucket;
    while cursor + Duration::minutes(1) <= now {
        let bar = candles.get(&cursor)?;
        if !bar.closed || bar.open_time + Duration::minutes(1) > now {
            return None;
        }
        cursor += Duration::minutes(1);
        if bar.high > high && bar.low < low {
            side = None;
            signal = None;
            continue;
        }
        if matches!(side, Some(Side::Sell)) && bar.low < low
            || matches!(side, Some(Side::Buy)) && bar.high > high
        {
            side = None;
            signal = None;
            continue;
        }
        if side.is_none() {
            if bar.high > high {
                side = Some(Side::Sell);
                extreme = bar.high;
            } else if bar.low < low {
                side = Some(Side::Buy);
                extreme = bar.low;
            }
        }
        let Some(direction) = side else { continue };
        extreme = match direction {
            Side::Sell => extreme.max(bar.high),
            Side::Buy => extreme.min(bar.low),
        };
        if low < bar.close && bar.close < high {
            let (entry_price, stop_price) = match direction {
                Side::Sell => (
                    quantize_up(high, tick_size),
                    quantize_up(extreme, tick_size) + tick_size,
                ),
                Side::Buy => (
                    quantize_down(low, tick_size),
                    quantize_down(extreme, tick_size) - tick_size,
                ),
            };
            signal = Some(Signal {
                side: direction,
                entry_price,
                stop_price,
                confirmed_at: cursor,
                range_start: bucket,
                range_low: low,
                range_high: high,
            });
            side = None;
        }
    }
    signal.filter(|s| now < s.expires_at())
}

pub fn invalid_reason(
    signal: &Signal,
    candle: Option<&Candle>,
    history: &BTreeMap<DateTime<Utc>, Candle>,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    if now >= signal.expires_at() || quarter_start(now) != signal.range_start {
        return Some("回踩挂单已到期");
    }
    let candle =
        match candle.filter(|c| c.open_time <= now && now < c.open_time + Duration::seconds(75)) {
            Some(candle) => candle,
            None => return Some("当前行情不可用，撤销开仓挂单"),
        };
    if history
        .values()
        .filter(|c| c.open_time >= signal.confirmed_at)
        .chain(std::iter::once(candle))
        .any(|c| match signal.side {
            Side::Sell => c.high >= signal.stop_price || c.low <= signal.range_low,
            Side::Buy => c.low <= signal.stop_price || c.high >= signal.range_high,
        })
    {
        return Some("信号已失效，撤销开仓挂单");
    }
    None
}

pub fn risk_reason(
    signal: &Signal,
    stop_pct: Decimal,
    take_profit_pct: Decimal,
) -> Option<&'static str> {
    if signal.entry_price <= Decimal::ZERO
        || signal.stop_price <= Decimal::ZERO
        || stop_pct <= Decimal::ZERO
        || take_profit_pct <= Decimal::ZERO
    {
        return Some("入场或保护参数无效");
    }
    let distance =
        (signal.entry_price - signal.stop_price).abs() / signal.entry_price * Decimal::from(100);
    if distance > stop_pct.min(take_profit_pct * Decimal::from(3)) {
        return Some("信号止损距离超出限制");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixture() -> (BTreeMap<DateTime<Utc>, Candle>, DateTime<Utc>) {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 8, 6, 0).unwrap();
        let start = now - Duration::minutes(66);
        let mut candles = BTreeMap::new();
        for i in 0..66 {
            let open_time = start + Duration::minutes(i);
            candles.insert(
                open_time,
                Candle {
                    open_time,
                    open: Decimal::new(9980, 2),
                    high: Decimal::from(100),
                    low: Decimal::new(9940, 2),
                    close: Decimal::new(9980, 2),
                    closed: true,
                },
            );
        }
        (candles, now)
    }

    #[test]
    fn matches_python_short_reclaim_price_and_expiry() {
        let (mut candles, now) = fixture();
        let last = candles.get_mut(&(now - Duration::minutes(1))).unwrap();
        last.high = Decimal::new(10005, 2);
        last.close = Decimal::new(9998, 2);
        let signal = find_reversal_signal(&candles, now, Decimal::new(1, 2)).unwrap();
        assert_eq!(signal.side, Side::Sell);
        assert_eq!(signal.entry_price, Decimal::from(100));
        assert_eq!(signal.stop_price, Decimal::new(10006, 2));
        assert_eq!(signal.confirmed_at, now);
        assert_eq!(signal.expires_at(), now + Duration::minutes(2));
        assert!(risk_reason(&signal, Decimal::ONE, Decimal::new(4, 2)).is_none());
    }

    #[test]
    fn rejects_missing_or_ambiguous_confirmation() {
        let (mut candles, now) = fixture();
        assert!(find_reversal_signal(&candles, now, Decimal::new(1, 2)).is_none());
        let last = candles.get_mut(&(now - Duration::minutes(1))).unwrap();
        last.high = Decimal::new(10005, 2);
        last.low = Decimal::new(9930, 2);
        last.close = Decimal::new(9998, 2);
        assert!(find_reversal_signal(&candles, now, Decimal::new(1, 2)).is_none());
        candles.remove(&(now - Duration::minutes(30)));
        assert!(find_reversal_signal(&candles, now, Decimal::new(1, 2)).is_none());
    }

    #[test]
    fn stale_live_candle_invalidates_entry() {
        let (mut candles, now) = fixture();
        let last = candles.get_mut(&(now - Duration::minutes(1))).unwrap();
        last.high = Decimal::new(10005, 2);
        last.close = Decimal::new(9998, 2);
        let signal = find_reversal_signal(&candles, now, Decimal::new(1, 2)).unwrap();
        assert!(invalid_reason(&signal, None, &candles, now).is_some());
    }
}
