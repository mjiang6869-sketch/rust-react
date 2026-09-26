//! 总览页的模拟盘收益序列。低频快照只用于研究展示，不参与下单与风控。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::{Json, extract::State};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use store::EquitySample;

use crate::dto::{ApiError, ApiResponse};
use crate::state::AppState;

const MAX_SAMPLES: usize = 10_000;
const MAX_CURVE_POINTS: usize = 400;

#[derive(Serialize)]
pub struct OverviewDto {
    source: &'static str,
    symbol: String,
    settlement_asset: String,
    as_of: DateTime<Utc>,
    session_started_at: DateTime<Utc>,
    equity: String,
    cumulative_pnl: String,
    cumulative_return_pct: Option<String>,
    realized_pnl: String,
    unrealized_pnl: String,
    estimated_month_pnl: Option<String>,
    estimated_annualized_pct: Option<String>,
    fee_is_authoritative: bool,
    sample_days: usize,
    curve: Vec<CurvePointDto>,
    daily: Vec<DailyPnlDto>,
}

#[derive(Serialize)]
struct CurvePointDto {
    at: DateTime<Utc>,
    equity: String,
}

#[derive(Serialize)]
struct DailyPnlDto {
    date: NaiveDate,
    realized_pnl: String,
    intensity: u8,
}

/// 不依赖行情连接：即使市场休市，也记录本次运行的账户基线。
pub fn spawn_sampling(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(StdDuration::from_secs(15 * 60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Err(error) = capture_sample(&state).await {
                tracing::warn!(%error, "账户权益快照保存失败");
            }
        }
    });
}

async fn capture_sample(state: &AppState) -> Result<(), store::StoreError> {
    if state.mode().await.is_live() {
        return Ok(());
    }
    let sample = {
        let engine = state.engine.lock().await;
        let snap = engine.snapshot();
        EquitySample {
            session_id: state.equity_session_id.clone(),
            sampled_at: Utc::now(),
            settlement_asset: engine.instrument().settlement_asset.clone(),
            equity: snap.equity,
            realized_pnl: snap.realized_pnl,
            unrealized_pnl: snap.unrealized_pnl,
        }
    };
    let conn = state.db.lock().await;
    store::insert_equity_sample(&conn, &sample)
}

pub async fn get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<OverviewDto>>, ApiError> {
    if state.mode().await.is_live() {
        return Err(ApiError::Conflict(
            "收益图仅显示模拟盘数据，实盘模式下不展示".into(),
        ));
    }
    let (current, initial_equity, fee_is_authoritative, symbol) = {
        let engine = state.engine.lock().await;
        let snap = engine.snapshot();
        (
            EquitySample {
                session_id: state.equity_session_id.clone(),
                sampled_at: Utc::now(),
                settlement_asset: engine.instrument().settlement_asset.clone(),
                equity: snap.equity,
                realized_pnl: snap.realized_pnl,
                unrealized_pnl: snap.unrealized_pnl,
            },
            engine.config().initial_equity,
            engine.instrument().fees.source.is_authoritative(),
            snap.symbol,
        )
    };
    let mut samples = {
        let conn = state.db.lock().await;
        store::recent_equity_samples(
            &conn,
            &state.equity_session_id,
            &current.settlement_asset,
            MAX_SAMPLES,
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?
    };
    if samples
        .last()
        .is_some_and(|last| last.sampled_at >= current.sampled_at)
    {
        samples.pop();
    }
    samples.push(current);
    Ok(Json(ApiResponse::ok(build_overview(
        &samples,
        initial_equity,
        fee_is_authoritative,
        symbol,
    ))))
}

fn local_date(at: DateTime<Utc>) -> NaiveDate {
    (at + Duration::hours(8)).date_naive()
}

fn consecutive_days(daily: &BTreeMap<NaiveDate, Decimal>, from: NaiveDate, to: NaiveDate) -> bool {
    let count = (to - from).num_days() + 1;
    count > 0 && daily.range(from..=to).count() == count as usize
}

fn curve_points(samples: &[EquitySample]) -> Vec<CurvePointDto> {
    let mut selected: Vec<&EquitySample> = Vec::new();
    if samples.len() <= MAX_CURVE_POINTS {
        selected.extend(samples);
    } else {
        selected.push(&samples[0]);
        let interior = &samples[1..samples.len() - 1];
        let bucket_size = interior.len().div_ceil((MAX_CURVE_POINTS - 2) / 2);
        for bucket in interior.chunks(bucket_size) {
            let mut low = 0;
            let mut high = 0;
            for (index, sample) in bucket.iter().enumerate().skip(1) {
                if sample.equity < bucket[low].equity {
                    low = index;
                }
                if sample.equity > bucket[high].equity {
                    high = index;
                }
            }
            if low <= high {
                selected.push(&bucket[low]);
                if high != low {
                    selected.push(&bucket[high]);
                }
            } else {
                selected.push(&bucket[high]);
                selected.push(&bucket[low]);
            }
        }
        selected.push(&samples[samples.len() - 1]);
    }
    selected
        .into_iter()
        .map(|sample| CurvePointDto {
            at: sample.sampled_at,
            equity: sample.equity.to_string(),
        })
        .collect()
}

fn build_overview(
    samples: &[EquitySample],
    initial_equity: Decimal,
    fee_is_authoritative: bool,
    symbol: String,
) -> OverviewDto {
    // 调用方至少附加一条当前快照；这里仍容忍空集，避免展示层查询影响交易链路。
    let first = samples.first();
    let last = samples.last();
    let today = last
        .map(|s| local_date(s.sampled_at))
        .unwrap_or_else(|| local_date(Utc::now()));
    let mut daily = BTreeMap::<NaiveDate, Decimal>::new();
    for sample in samples {
        daily
            .entry(local_date(sample.sampled_at))
            .or_insert(Decimal::ZERO);
    }
    for pair in samples.windows(2) {
        let delta = pair[1].realized_pnl - pair[0].realized_pnl;
        *daily
            .entry(local_date(pair[1].sampled_at))
            .or_insert(Decimal::ZERO) += delta;
    }

    let equity = last.map_or(initial_equity, |s| s.equity);
    let cumulative_pnl = equity - initial_equity;
    let cumulative_return_pct = (initial_equity > Decimal::ZERO).then(|| {
        (cumulative_pnl / initial_equity * Decimal::from(100))
            .round_dp(2)
            .to_string()
    });

    let month_start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today);
    let first_month_day = daily
        .range(month_start..=today)
        .next()
        .map(|(date, _)| *date);
    let month_days = first_month_day.map_or(0, |start| (today - start).num_days() + 1);
    let month_pnl: Decimal = daily.range(month_start..=today).map(|(_, pnl)| *pnl).sum();
    let next_month = if today.month() == 12 {
        NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)
    };
    let days_in_month = next_month.map_or(0, |date| (date - month_start).num_days());
    let estimated_month_pnl = (fee_is_authoritative
        && month_days >= 7
        && days_in_month > 0
        && first_month_day.is_some_and(|start| consecutive_days(&daily, start, today)))
    .then(|| {
        (month_pnl * Decimal::from(days_in_month) / Decimal::from(month_days))
            .round_dp(2)
            .to_string()
    });

    let trailing_start = today - Duration::days(29);
    let trailing_pnl: Decimal = daily
        .range(trailing_start..=today)
        .map(|(_, pnl)| *pnl)
        .sum();
    let estimated_annualized_pct = (fee_is_authoritative
        && initial_equity > Decimal::ZERO
        && consecutive_days(&daily, trailing_start, today))
    .then(|| {
        (trailing_pnl / initial_equity * Decimal::from(365) / Decimal::from(30)
            * Decimal::from(100))
        .round_dp(2)
        .to_string()
    });

    let curve = curve_points(samples);

    let max_daily_pnl = daily
        .values()
        .map(|value| value.abs())
        .max()
        .unwrap_or(Decimal::ZERO);

    OverviewDto {
        source: "paper_account_snapshots",
        symbol,
        settlement_asset: last.map_or("", |s| s.settlement_asset.as_str()).to_owned(),
        as_of: last.map_or_else(Utc::now, |s| s.sampled_at),
        session_started_at: first.map_or_else(Utc::now, |s| s.sampled_at),
        equity: equity.to_string(),
        cumulative_pnl: cumulative_pnl.to_string(),
        cumulative_return_pct,
        realized_pnl: last.map_or(Decimal::ZERO, |s| s.realized_pnl).to_string(),
        unrealized_pnl: last.map_or(Decimal::ZERO, |s| s.unrealized_pnl).to_string(),
        estimated_month_pnl,
        estimated_annualized_pct,
        fee_is_authoritative,
        sample_days: daily.len(),
        curve,
        daily: daily
            .into_iter()
            .map(|(date, pnl)| {
                let intensity = if pnl.is_zero() || max_daily_pnl.is_zero() {
                    0
                } else if pnl.abs() * Decimal::from(4) >= max_daily_pnl * Decimal::from(3) {
                    4
                } else if pnl.abs() * Decimal::from(2) >= max_daily_pnl {
                    3
                } else if pnl.abs() * Decimal::from(4) >= max_daily_pnl {
                    2
                } else {
                    1
                };
                DailyPnlDto {
                    date,
                    realized_pnl: pnl.to_string(),
                    intensity,
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn calendar_uses_realized_deltas_and_projections_need_trustworthy_coverage() {
        let start = DateTime::parse_from_rfc3339("2026-09-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let samples: Vec<_> = (0..30)
            .map(|day| EquitySample {
                session_id: "run".into(),
                sampled_at: start + Duration::days(day),
                settlement_asset: "USDC".into(),
                equity: dec!(10000) + Decimal::from(day),
                realized_pnl: Decimal::from(day),
                unrealized_pnl: Decimal::ZERO,
            })
            .collect();
        let full = build_overview(&samples, dec!(10000), true, "ETHUSDC".into());
        assert_eq!(full.daily[0].realized_pnl, "0");
        assert_eq!(full.daily[1].realized_pnl, "1");
        assert_eq!(full.cumulative_pnl, "29");
        assert!(full.estimated_month_pnl.is_some());
        assert!(full.estimated_annualized_pct.is_some());

        let assumed_fee = build_overview(&samples, dec!(10000), false, "ETHUSDC".into());
        assert!(assumed_fee.estimated_month_pnl.is_none());
        assert!(assumed_fee.estimated_annualized_pct.is_none());
        let short = build_overview(&samples[..3], dec!(10000), true, "ETHUSDC".into());
        assert!(short.estimated_month_pnl.is_none());
        assert!(short.estimated_annualized_pct.is_none());

        let mut with_gap = samples.clone();
        with_gap.retain(|sample| local_date(sample.sampled_at).day() != 15);
        let incomplete = build_overview(&with_gap, dec!(10000), true, "ETHUSDC".into());
        assert!(incomplete.estimated_month_pnl.is_none());
        assert!(incomplete.estimated_annualized_pct.is_none());
    }

    #[test]
    fn curve_downsampling_preserves_brief_drawdown_and_peak() {
        let start = DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let samples: Vec<_> = (0..500)
            .map(|index| EquitySample {
                session_id: "run".into(),
                sampled_at: start + Duration::minutes(index),
                settlement_asset: "USDC".into(),
                equity: match index {
                    271 => dec!(9000),
                    272 => dec!(11000),
                    _ => dec!(10000),
                },
                realized_pnl: Decimal::ZERO,
                unrealized_pnl: Decimal::ZERO,
            })
            .collect();
        let curve = curve_points(&samples);
        assert!(curve.len() <= MAX_CURVE_POINTS);
        assert_eq!(
            curve.first().map(|point| point.at),
            Some(samples[0].sampled_at)
        );
        assert_eq!(
            curve.last().map(|point| point.at),
            Some(samples[499].sampled_at)
        );
        assert!(curve.iter().any(|point| point.equity == "9000"));
        assert!(curve.iter().any(|point| point.equity == "11000"));
    }
}
