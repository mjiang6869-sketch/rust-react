//! 回测任务。
//!
//! # 与 CLI 的 `backtest` 子命令共用逻辑
//!
//! 这里不重新实现回测——它调用 `sim::run`，与 CLI 完全一致。差别只在输出
//! 形式：CLI 打印表格，这里返回 JSON。
//!
//! # 结论可信度必须一起返回
//!
//! `VerdictDto` 不是可选字段。界面必须显眼展示它——只给 P&L 而不给
//! "这个结论有多可信"，会让人把依赖费率活动的结果当成真实 edge。

use std::path::Path;

use anyhow::{Result, bail};
use chrono::{Datelike, Duration, Months, NaiveDate};
use domain::{FeeSource, Instrument, MarketEvent, RiskLimits};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::dto::{BacktestResultDto, EquityPointDto, ModelResultDto, TradeDto, VerdictDto};

/// 一次回测请求的全部参数。
///
/// 独立成类型而非散参数：九个位置参数里有两个是日期、一个是列表，
/// 传错顺序编译器不会拦，而日期传反会静默得到空结果而不是报错。
pub struct BacktestRequest<'a> {
    pub data_root: &'a Path,
    pub instrument: &'a Instrument,
    pub symbol: &'a str,
    pub strategy_id: &'a str,
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// 成交模型列表，例如 `["m0", "m1"]`。
    pub models: &'a [String],
    pub initial_equity: Decimal,
    pub limits: RiskLimits,
}

/// 跑一次回测。
pub fn run(req: &BacktestRequest<'_>) -> Result<BacktestResultDto> {
    run_with_progress(req, |_stage, _done, _total| {})
}

/// 与 `run` 相同，但在每个数据分片和成交模型完成时回调进度。
pub fn run_with_progress<F>(req: &BacktestRequest<'_>, mut progress: F) -> Result<BacktestResultDto>
where
    F: FnMut(&str, usize, usize),
{
    let BacktestRequest {
        data_root,
        instrument,
        symbol,
        strategy_id,
        from,
        to,
        models,
        initial_equity,
        limits,
    } = req;
    let (from, to, initial_equity, limits) = (*from, *to, *initial_equity, *limits);

    if from > to {
        bail!("起始日期不能晚于结束日期");
    }

    let strategy =
        strategies::by_id(strategy_id).ok_or_else(|| anyhow::anyhow!("未知策略：{strategy_id}"))?;

    let total_days = (to - from).num_days().max(0) as usize + 1;
    let (events, missing) = load_events(data_root, symbol, from, to, |done| {
        progress("加载数据", done, total_days);
    })?;
    if events.is_empty() {
        bail!(
            "没有读到 {symbol} 在 {from} .. {to} 的数据。\
             请先在数据管理里下载对应区间。"
        );
    }

    let trades: Vec<sim::Trade> = events
        .iter()
        .filter_map(|e| match e {
            MarketEvent::AggTrade(t) => Some(sim::Trade::from(*t)),
            _ => None,
        })
        .collect();
    let tape = sim::liquidity::TradeTape::from_trades(trades);

    let candle_count = events
        .iter()
        .filter(|e| matches!(e, MarketEvent::Kline(_)))
        .count();

    // 逐模型跑
    let mut results = Vec::new();
    for (model_index, name) in models.iter().enumerate() {
        let model =
            sim::model_by_name(name).ok_or_else(|| anyhow::anyhow!("未知成交模型：{name}"))?;
        let config = sim::BacktestConfig {
            instrument: (*instrument).clone(),
            limits,
            initial_equity,
            lookback: 60,
            assumed_latency_ms: 100,
            allow_gaps: false,
            fee_source: instrument.fees.source,
            standard_maker_rate: Decimal::new(2, 4),
        };
        let r = sim::run(&config, &events, &tape, strategy.as_ref(), model.as_ref());

        let final_equity = r
            .equity_curve
            .last()
            .map(|p| p.equity)
            .unwrap_or(initial_equity);
        let wins = r.trades.iter().filter(|t| t.pnl > Decimal::ZERO).count();
        let win_rate = if r.trades.is_empty() {
            None
        } else {
            Some((Decimal::from(wins) / Decimal::from(r.trades.len())).to_string())
        };

        results.push(ModelRun {
            name: model.name().to_string(),
            optimism: match model.optimism() {
                sim::Optimism::UpperBound => "UPPER_BOUND",
                sim::Optimism::ConservativeLower => "CONSERVATIVE_LOWER",
            },
            final_equity,
            pnl: final_equity - initial_equity,
            trade_count: r.trades.len(),
            win_rate,
            result: r,
        });
        progress(name, model_index + 1, models.len());
    }

    // 构造裁决
    let verdict = build_verdict(&results, initial_equity, !missing.is_empty());

    let model_dtos: Vec<ModelResultDto> = results
        .iter()
        .map(|m| {
            let last_at = m.result.equity_curve.last().map(|p| p.at);
            let first_at = m.result.equity_curve.first().map(|p| p.at);
            let annualized_return = match (
                first_at,
                last_at,
                m.final_equity.to_f64(),
                initial_equity.to_f64(),
            ) {
                (Some(start), Some(end), Some(final_value), Some(initial)) if initial > 0.0 => {
                    let days = (end - start).num_seconds().max(1) as f64 / 86_400.0;
                    Some(format!(
                        "{:.8}",
                        (final_value / initial).powf(365.0 / days) - 1.0
                    ))
                }
                _ => None,
            };
            ModelResultDto {
                name: m.name.clone(),
                optimism: m.optimism,
                final_equity: m.final_equity.to_string(),
                pnl: m.pnl.to_string(),
                trade_count: m.trade_count,
                win_rate: m.win_rate.clone(),
                cumulative_pnl: m.pnl.to_string(),
                annualized_return,
                liquidated: m.result.termination_reason.as_deref() == Some("LIQUIDATED"),
                termination_reason: m.result.termination_reason.clone(),
                equity_curve: m
                    .result
                    .equity_curve
                    .iter()
                    .map(|p| EquityPointDto {
                        at: p.at,
                        equity: p.equity.to_string(),
                    })
                    .collect(),
                trades: m
                    .result
                    .trades
                    .iter()
                    .map(|t| TradeDto {
                        entry_at: t.entry_at,
                        exit_at: t.exit_at,
                        side: match t.side {
                            domain::Side::Buy => "BUY",
                            domain::Side::Sell => "SELL",
                        }
                        .into(),
                        quantity: t.quantity.to_string(),
                        entry_price: t.entry_price.to_string(),
                        exit_price: t.exit_price.to_string(),
                        fee: t.fee.to_string(),
                        exit_reason: match t.exit_reason {
                            sim::ExitKind::TakeProfit => "TAKE_PROFIT",
                            sim::ExitKind::StopLoss => "STOP_LOSS",
                            sim::ExitKind::Liquidation => "LIQUIDATION",
                            sim::ExitKind::ForcedAtEnd => "FORCED_AT_END",
                        }
                        .into(),
                        pnl: t.pnl.to_string(),
                    })
                    .collect(),
            }
        })
        .collect();

    Ok(BacktestResultDto {
        symbol: symbol.to_string(),
        strategy_id: strategy_id.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        candle_count,
        models: model_dtos,
        verdict,
    })
}

struct ModelRun {
    name: String,
    optimism: &'static str,
    final_equity: Decimal,
    pnl: Decimal,
    trade_count: usize,
    win_rate: Option<String>,
    result: sim::BacktestResult,
}

/// 构造结论可信度。
///
/// 优先级：符号翻转 > 费率不完整 > 逆向选择 > 成交率赌注 > 稳健。
/// 这个顺序不是随意的——越靠前的条件越根本，后面的结论在它不成立时
/// 没有意义。
fn build_verdict(
    results: &[ModelRun],
    initial_equity: Decimal,
    has_missing_data: bool,
) -> VerdictDto {
    let optimistic = results.iter().find(|m| m.optimism == "UPPER_BOUND");
    let conservative = results.iter().find(|m| m.optimism == "CONSERVATIVE_LOWER");

    // 两个模型都在时才能做对比
    let (sign_flips, breakeven, markout, stop_events, max_exposure) =
        match (optimistic, conservative) {
            (Some(o), Some(c)) => {
                let m = sim::compare_models(&o.result, &c.result);
                (
                    m.sign_flips,
                    m.breakeven_fill_rate,
                    Some(m.markout),
                    m.stop_exposure.events,
                    m.stop_exposure.max_exposure_secs,
                )
            }
            _ => (false, None, None, 0, 0),
        };

    let conservative_run = conservative.or_else(|| results.last());
    let fee_incomplete = conservative_run
        .map(|m| m.result.provenance.fees.incomplete)
        .unwrap_or(true);

    // 先算 adverse（借用），后面还要用 markout 的字段（移动）。
    let adverse = markout
        .as_ref()
        .is_some_and(|m| m.has_systematic_adverse_selection());

    let conclusive = !sign_flips
        && !fee_incomplete
        && !adverse
        && !has_missing_data
        && breakeven.is_some_and(|r| r < Decimal::new(8, 1));

    let message = if sign_flips {
        "结论不可信：乐观模型与诚实模型给出相反的盈亏方向。策略依赖不现实的成交假设。".to_string()
    } else if has_missing_data {
        "结论不完整：回测区间存在数据缺口，跨越缺口会凭空发明成交。".to_string()
    } else if fee_incomplete {
        "结论不完整：费率来源未经交易所账户对账，而整个 edge 依赖零费率活动。".to_string()
    } else if adverse {
        "存在系统性逆向选择：成交后价格平均朝不利方向走，零手续费也难以盈利。".to_string()
    } else if breakeven.is_some_and(|r| r >= Decimal::new(8, 1)) {
        "这是成交率赌注而非做市 edge：需要实现乐观模型 80% 以上的成交量才能不亏。".to_string()
    } else if breakeven.is_none() {
        "乐观模型下即不盈利，策略本身没有 edge。".to_string()
    } else {
        "结论相对稳健：诚实模型下仍盈利，且无系统性逆向选择。".to_string()
    };

    let promo_pnl = conservative_run.map(|m| m.pnl).unwrap_or(Decimal::ZERO);
    let standard_pnl = conservative_run
        .map(|m| m.result.final_equity_at_standard_fee - initial_equity)
        .unwrap_or(Decimal::ZERO);

    VerdictDto {
        conclusive,
        message,
        sign_flips,
        breakeven_fill_rate: breakeven.map(|v| v.to_string()),
        markout_5s: markout.map(|m| m.mean_5s.to_string()),
        fee_incomplete,
        stop_exposure_events: stop_events,
        max_exposure_secs: max_exposure,
        pnl_at_promotional_fee: promo_pnl.to_string(),
        pnl_at_standard_fee: standard_pnl.to_string(),
    }
}

/// 读取区间内的事件。
///
/// 与 CLI 相同：按天遍历、用 `data::replay` 读 Parquet。
fn load_events(
    data_root: &Path,
    symbol: &str,
    from: NaiveDate,
    to: NaiveDate,
    mut progress: impl FnMut(usize),
) -> Result<(Vec<MarketEvent>, Vec<String>)> {
    let mut all = Vec::new();
    let mut missing = Vec::new();
    let mut done = 0usize;

    // 以月为边界组织读取。`monthly` 在每次循环结束时被释放，避免把
    // Parquet 解压出的成交带和最终事件数组同时扩张到多个临时副本。
    let mut month_start = from.with_day(1).expect("合法日期");
    while month_start <= to {
        let next_month = month_start
            .checked_add_months(Months::new(1))
            .expect("日期范围过大");
        let month_end = (next_month - Duration::days(1)).min(to);
        let mut monthly = Vec::new();
        let mut day = from.max(month_start);
        while day <= month_end {
            match data::replay::load_day(
                data_root,
                symbol,
                "1m",
                day.year(),
                day.month(),
                day.day(),
            ) {
                Ok(slice) => {
                    if slice.candles.is_empty() && slice.trades.is_empty() {
                        missing.push(day.to_string());
                    } else {
                        monthly.extend(slice.into_events());
                    }
                }
                Err(_) => missing.push(day.to_string()),
            }
            done += 1;
            progress(done);
            day += Duration::days(1);
        }
        monthly.sort_by_key(|e| e.at());
        all.extend(monthly);
        month_start = next_month;
    }

    all.sort_by_key(|e| e.at());
    Ok((all, missing))
}

/// 把 `FeeSource` 转成展示标签。
pub fn fee_source_tag(s: FeeSource) -> &'static str {
    match s {
        FeeSource::ExchangeAccount => "EXCHANGE_ACCOUNT",
        FeeSource::ExchangeRules => "EXCHANGE_RULES",
        FeeSource::PromotionalAssumed => "PROMOTIONAL_ASSUMED",
        FeeSource::ConfiguredDefault => "CONFIGURED_DEFAULT",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn unknown_strategy_is_rejected() {
        let inst = test_instrument();
        let r = run(&BacktestRequest {
            data_root: Path::new("/nonexistent"),
            instrument: &inst,
            symbol: "ETHUSDC",
            strategy_id: "nonexistent_strategy",
            from: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            to: NaiveDate::from_ymd_opt(2026, 8, 2).unwrap(),
            models: &["m1".to_string()],
            initial_equity: dec!(10000),
            limits: RiskLimits::default(),
        });
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("未知策略"));
    }

    #[test]
    fn unknown_fill_model_is_rejected() {
        let inst = test_instrument();
        let r = run(&BacktestRequest {
            data_root: Path::new("/nonexistent"),
            instrument: &inst,
            symbol: "ETHUSDC",
            strategy_id: "range_maker",
            from: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            to: NaiveDate::from_ymd_opt(2026, 8, 2).unwrap(),
            models: &["m99".to_string()],
            initial_equity: dec!(10000),
            limits: RiskLimits::default(),
        });
        assert!(r.is_err());
    }

    #[test]
    fn reversed_date_range_is_rejected() {
        let inst = test_instrument();
        let r = run(&BacktestRequest {
            data_root: Path::new("/nonexistent"),
            instrument: &inst,
            symbol: "ETHUSDC",
            strategy_id: "range_maker",
            from: NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
            to: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            models: &["m1".to_string()],
            initial_equity: dec!(10000),
            limits: RiskLimits::default(),
        });
        let e = r.unwrap_err().to_string();
        assert!(e.contains("晚于"), "{e}");
    }

    /// 没有数据时必须给出可操作的提示，而不是空结果。
    #[test]
    fn missing_data_gives_actionable_error() {
        let inst = test_instrument();
        let r = run(&BacktestRequest {
            data_root: Path::new("/nonexistent-data-root"),
            instrument: &inst,
            symbol: "ETHUSDC",
            strategy_id: "range_maker",
            from: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            to: NaiveDate::from_ymd_opt(2026, 8, 2).unwrap(),
            models: &["m1".to_string()],
            initial_equity: dec!(10000),
            limits: RiskLimits::default(),
        });
        let e = r.unwrap_err().to_string();
        assert!(e.contains("没有读到"), "{e}");
        assert!(e.contains("下载"), "错误应提示如何解决：{e}");
    }

    /// 费率来源标签要与数据库里的表示一致。
    #[test]
    fn fee_source_tags_are_stable() {
        assert_eq!(
            fee_source_tag(FeeSource::PromotionalAssumed),
            "PROMOTIONAL_ASSUMED"
        );
        assert_eq!(
            fee_source_tag(FeeSource::ExchangeAccount),
            "EXCHANGE_ACCOUNT"
        );
    }

    fn test_instrument() -> Instrument {
        use domain::{ContractKind, FeeSchedule, Precision};
        Instrument {
            symbol: "ETHUSDC".into(),
            kind: ContractKind::CryptoPerp,
            base_asset: "ETH".into(),
            quote_asset: "USDC".into(),
            margin_asset: "USDC".into(),
            settlement_asset: "USDC".into(),
            precision: Precision {
                tick_size: dec!(0.01),
                step_size: dec!(0.001),
                min_qty: dec!(0.001),
                min_notional: dec!(5),
            },
            maint_margin_pct: dec!(2.5),
            required_margin_pct: dec!(5),
            liquidation_fee: dec!(0.0125),
            fees: FeeSchedule {
                maker_rate: Decimal::ZERO,
                taker_rate: dec!(0.0005),
                source: FeeSource::PromotionalAssumed,
                observed_at: chrono::Utc::now(),
            },
        }
    }
}
