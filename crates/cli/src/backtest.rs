//! `rc backtest` —— 用本地数据跑回测并打印对比表。

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use data::replay;
use domain::{ContractKind, FeeSchedule, FeeSource, Instrument, MarketEvent, Precision};
use rust_decimal::Decimal;
use sim::liquidity::TradeTape;
use sim::{BacktestConfig, BacktestResult, EdgeMetrics, FillModel, Optimism};

use crate::format;
use crate::{data_root, flag_list, flag_one, flag_or, flag_parse, parse_date};

pub async fn run(args: &[String]) -> Result<()> {
    let symbol = flag_one(args, "--symbol").context("必须指定 --symbol")?;
    let strategy_id = flag_or(args, "--strategy", "range_maker");
    let from = parse_date(&flag_one(args, "--from").context("必须指定 --from YYYY-MM-DD")?)?;
    let to = parse_date(&flag_one(args, "--to").context("必须指定 --to YYYY-MM-DD")?)?;
    if from > to {
        bail!("--from 不能晚于 --to");
    }

    let model_names = {
        let l = flag_list(args, "--fill-models");
        if l.is_empty() {
            vec!["m0".to_string(), "m1".to_string()]
        } else {
            l
        }
    };

    let equity: Decimal = flag_parse(args, "--equity")?.unwrap_or(Decimal::from(10_000));
    let root = data_root(args);

    // ---- 1. 构造合约规则 ----
    //
    // 这些值应从 exchangeInfo 读取。当前用实测值硬编码，并在输出里明确标注
    // 来源，避免让人误以为是账户真实费率。
    let instrument = instrument_for(&symbol)?;

    // ---- 2. 读数据 ----
    println!("读取数据 {symbol} {from} .. {to}");
    let started = std::time::Instant::now();
    let (events, missing) = load_events(&root, &symbol, from, to)?;
    if events.is_empty() {
        bail!(
            "没有读到任何数据。请先下载：\n  rc download --symbol {symbol} \
             --kind klines,agg_trades --from {}-{:02} --to {}-{:02}",
            from.year(),
            from.month(),
            to.year(),
            to.month()
        );
    }
    println!(
        "  读到 {} 个行情事件（{} 根 K 线），耗时 {}s",
        events.len(),
        events
            .iter()
            .filter(|e| matches!(e, MarketEvent::Kline(_)))
            .count(),
        started.elapsed().as_secs()
    );
    if !missing.is_empty() {
        println!("  缺失 {} 个数据分片", missing.len());
    }
    println!();

    // ---- 3. 构造成交带 ----
    //
    // 注意这里把整个区间的成交一次性装入内存。32 GB 机器上，一个月约
    // 1400 万笔、约 1.5 GB 是可接受的。区间更长时需要改成按天分片回放，
    // 这里先保持实现直观。
    let trades: Vec<sim::Trade> = events
        .iter()
        .filter_map(|e| match e {
            MarketEvent::AggTrade(t) => Some(sim::Trade::from(*t)),
            _ => None,
        })
        .collect();
    let tape = TradeTape::from_trades(trades);
    println!("成交带：{} 笔", tape.len());

    // ---- 4. 逐模型跑回测 ----
    let strategy = strategies::by_id(&strategy_id)
        .with_context(|| format!("未知策略：{strategy_id}。用 `rc strategies` 查看可用策略"))?;

    let mut results: Vec<(String, Box<dyn FillModel>, BacktestResult)> = Vec::new();
    for name in &model_names {
        let model = sim::model_by_name(name)
            .with_context(|| format!("未知成交模型：{name}。用 `rc models` 查看"))?;
        let config = BacktestConfig {
            instrument: instrument.clone(),
            limits: domain::RiskLimits::default(),
            initial_equity: equity,
            lookback: 60,
            assumed_latency_ms: 100,
            allow_gaps: false,
            fee_source: FeeSource::PromotionalAssumed,
            standard_maker_rate: Decimal::new(2, 4), // 0.02%
        };
        let started = std::time::Instant::now();
        let r = sim::run(&config, &events, &tape, strategy.as_ref(), model.as_ref());
        println!(
            "  模型 {} 跑完：{} 笔交易，耗时 {}s",
            model.name(),
            r.trades.len(),
            started.elapsed().as_secs()
        );
        results.push((name.clone(), model, r));
    }

    println!();
    print_report(&symbol, &strategy_id, &instrument, &results, equity);

    Ok(())
}

/// 从交易所规则构造合约。
///
/// **当前是硬编码**。等接入 `exchange` crate 的 `exchangeInfo` 后改为实时读取。
/// 输出里会明确标注来源，避免把这些值误认为账户真实费率。
fn instrument_for(symbol: &str) -> Result<Instrument> {
    // 实测值（2026-09 从 exchangeInfo 读取）。
    let (kind, quote, base, tick, step) = if symbol.ends_with("USDC") {
        (
            ContractKind::CryptoPerp,
            "USDC",
            symbol.trim_end_matches("USDC"),
            Decimal::new(1, 2), // 0.01
            Decimal::new(1, 3), // 0.001
        )
    } else {
        (
            ContractKind::TradFiPerp,
            "USDT",
            symbol.trim_end_matches("USDT"),
            Decimal::new(1, 2),
            Decimal::new(1, 3),
        )
    };

    Ok(Instrument {
        symbol: symbol.to_string(),
        kind,
        base_asset: base.to_string(),
        quote_asset: quote.to_string(),
        margin_asset: quote.to_string(),
        settlement_asset: quote.to_string(),
        precision: Precision {
            tick_size: tick,
            step_size: step,
            min_qty: step,
            min_notional: Decimal::from(5),
        },
        // 来自 exchangeInfo 的 maintMarginPercent。旧实现硬编码 0.4%，
        // 导致高杠杆下误判止损不安全并静默拒绝信号。
        maint_margin_pct: Decimal::new(25, 1), // 2.5
        required_margin_pct: Decimal::from(5),
        liquidation_fee: Decimal::new(125, 4), // 0.0125
        fees: FeeSchedule {
            maker_rate: Decimal::ZERO,      // 零费率活动
            taker_rate: Decimal::new(5, 4), // 0.05%
            // 活动费率尚未与账户对账，所以标记为假设。回测结果会因此
            // 被标记为不完整。
            source: FeeSource::PromotionalAssumed,
            observed_at: Utc::now(),
        },
    })
}

/// 读取区间内所有日期的事件。
fn load_events(
    root: &std::path::Path,
    symbol: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(Vec<MarketEvent>, Vec<String>)> {
    let mut all_events = Vec::new();
    let mut missing = Vec::new();

    let mut day = from;
    while day <= to {
        let (y, m, d) = (day.year(), day.month(), day.day());
        match replay::load_day(root, symbol, "1m", y, m, d) {
            Ok(slice) => {
                if slice.candles.is_empty() && slice.trades.is_empty() {
                    missing.push(format!("{day}"));
                } else {
                    all_events.extend(slice.into_events());
                }
            }
            Err(_) => missing.push(format!("{day}")),
        }
        day += Duration::days(1);
    }

    all_events.sort_by_key(|e| e.at());
    Ok((all_events, missing))
}

use chrono::Datelike;

/// 打印对比报告。
fn print_report(
    symbol: &str,
    strategy_id: &str,
    instrument: &Instrument,
    results: &[(String, Box<dyn FillModel>, BacktestResult)],
    initial_equity: Decimal,
) {
    println!("{}", "=".repeat(72));
    println!("回测报告");
    println!("{}", "=".repeat(72));

    // ---- 基本信息 ----
    if let Some((_, _, first)) = results.first() {
        let p = &first.provenance;
        format::kv("交易对", symbol);
        format::kv("策略", strategy_id);
        format::kv("区间", &format!("{} .. {}", p.start, p.end));
        format::kv("K 线数", &p.candle_count.to_string());
        format::kv("初始权益", &format::dec(initial_equity, 2));
        format::kv(
            "结算资产",
            &format!(
                "{}（保证金 {}）",
                instrument.settlement_asset, instrument.margin_asset
            ),
        );
        format::kv(
            "费率",
            &format!(
                "maker {} / taker {}（{:?}）",
                instrument.fees.maker_rate, instrument.fees.taker_rate, instrument.fees.source
            ),
        );
        format::kv(
            "维持保证金率",
            &format::pct(instrument.maint_margin_pct / Decimal::from(100)),
        );
        if p.fees.incomplete {
            println!();
            println!("  ⚠ 费率来源为「零费率活动假设」，未经交易所账户对账。");
            println!("    下方结果标记为不完整——活动结束或未覆盖该合约时结论会变。");
        }
    }

    // ---- 交易明细特征 ----
    if let Some((_, _, c)) = conservative_of(results) {
        if !c.trades.is_empty() {
            let n = Decimal::from(c.trades.len());
            let wins = c.trades.iter().filter(|t| t.pnl > Decimal::ZERO).count();
            let mean_pnl: Decimal = c.trades.iter().map(|t| t.pnl).sum::<Decimal>() / n;
            let mean_entry: Decimal = c.trades.iter().map(|t| t.entry_price).sum::<Decimal>() / n;
            // 平均止盈距离：从成交记录里反推不是好办法，改用首笔的名义收益率
            let mean_ret = c
                .trades
                .iter()
                .map(|t| {
                    if t.entry_price > Decimal::ZERO {
                        (t.exit_price - t.entry_price).abs() / t.entry_price
                    } else {
                        Decimal::ZERO
                    }
                })
                .sum::<Decimal>()
                / n;

            println!("\n{}", "-".repeat(72));
            println!("交易特征");
            println!("{}", "-".repeat(72));
            format::kv("交易笔数", &c.trades.len().to_string());
            format::kv("胜率", &format::pct(Decimal::from(wins) / n));
            format::kv("单笔平均盈亏", &format::signed(mean_pnl.round_dp(4)));
            format::kv("平均入场价", &format::dec(mean_entry, 2));
            format::kv("平均出场距离", &format::bp(mean_ret));
            let reasons = count_exit_kinds(&c.trades);
            format::tally("出场方式分布", &reasons);
        }
    }

    // ---- 各模型结果 ----
    println!("\n{}", "-".repeat(72));
    println!("各成交模型结果");
    println!("{}", "-".repeat(72));
    println!(
        "  {:<26} {:>12} {:>9} {:>10}",
        "模型", "最终权益", "交易数", "结论"
    );

    for (name, model, r) in results {
        let final_eq = r
            .equity_curve
            .last()
            .map(|p| p.equity)
            .unwrap_or(initial_equity);
        let optimism = match model.optimism() {
            Optimism::UpperBound => "上界",
            Optimism::ConservativeLower => "下界",
        };
        let pnl = final_eq - initial_equity;
        println!(
            "  {:<26} {:>12} {:>9} {:>10}",
            format!("{} ({optimism})", model.name()),
            format::signed(pnl.round_dp(2)),
            r.trades.len(),
            if pnl > Decimal::ZERO {
                "盈利"
            } else {
                "亏损"
            }
        );
        let _ = name;
    }

    // ---- 跨模型对比（反自欺核心）----
    let optimistic = results
        .iter()
        .find(|(_, m, _)| m.optimism() == Optimism::UpperBound);
    let conservative = results
        .iter()
        .find(|(_, m, _)| m.optimism() == Optimism::ConservativeLower);

    if let (Some((_, _, o)), Some((_, _, c))) = (optimistic, conservative) {
        print_verdict(o, c, initial_equity);
    } else {
        println!("\n  提示：只跑了单一模型。同时跑 m0 与 m1 才能判断结论是否依赖");
        println!("        不现实的成交假设——用 --fill-models m0,m1。");
    }

    // ---- 止损裸露（maker-only 特有风险）----
    if let Some((_, _, c)) = conservative.or(results.last()) {
        let s = &c.metrics.stop_exposure;
        if s.events > 0 {
            println!("\n{}", "-".repeat(72));
            println!("止损裸露分析（maker-only 特有风险）");
            println!("{}", "-".repeat(72));
            println!("  止损是挂单，跳空穿过它且不回来时仓位持续裸露。");
            println!();
            format::kv("触发未成交次数", &s.events.to_string());
            format::kv("最终以止损价附近成交", &format::pct(s.filled_ratio));
            format::kv("走到强平", &format::pct(s.liquidated_ratio));
            format::kv("裸露时长中位数", &format::duration(s.median_exposure_secs));
            format::kv("裸露时长最大值", &format::duration(s.max_exposure_secs));
            format::kv("平均滑点", &format::dec(s.mean_slippage, 4));
            if s.liquidated_ratio > Decimal::ZERO {
                println!();
                println!("  ⚠ 有止损未能成交并走到强平。这是该策略最大的风险敞口。");
            }
        }
    }

    // ---- 延迟拒单 ----
    if let Some((_, _, c)) = conservative.or(results.last()) {
        let l = &c.metrics.latency;
        if l.attempts > 0 {
            println!("\n{}", "-".repeat(72));
            println!("延迟与 post-only 拒单");
            println!("{}", "-".repeat(72));
            format::kv("提交尝试", &l.attempts.to_string());
            format::kv("post-only 被拒", &l.rejected_post_only.to_string());
            format::kv("拒单率", &format::pct(l.rejection_ratio()));
            format::kv("假设延迟", &format!("{}ms", l.assumed_latency_ms));
            if l.rejection_ratio() > Decimal::new(2, 1) {
                println!();
                println!("  拒单率偏高：价格在延迟窗口内已穿过我们的价位。");
                println!("  这会让挂单频繁落空，实际成交率低于回测假设。");
            }
        }
    }

    // ---- markout（逆向选择）----
    if let Some((_, _, c)) = conservative.or(results.last()) {
        let m = &c.metrics.markout;
        if m.samples > 0 {
            println!("\n{}", "-".repeat(72));
            println!("Markout 分析（逆向选择检测）");
            println!("{}", "-".repeat(72));
            println!("  成交后价格朝哪个方向走。正数对我们有利，负数是被逆向选择。");
            println!("  这个指标比 P&L 更重要——若系统性为负，零手续费也救不了。");
            println!();
            format::kv("样本数", &m.samples.to_string());
            format::kv("+1s 均值", &format::signed(m.mean_1s.round_dp(4)));
            format::kv("+5s 均值", &format::signed(m.mean_5s.round_dp(4)));
            format::kv("+30s 均值", &format::signed(m.mean_30s.round_dp(4)));
            format::kv("+5m 均值", &format::signed(m.mean_5m.round_dp(4)));
            format::kv("5s 逆向比例", &format::pct(m.adverse_ratio_5s));
            if m.has_systematic_adverse_selection() {
                println!();
                println!("  ⚠ 5 秒 markout 均值为负 = 系统性逆向选择。");
            }
        }
    }

    // ---- 费率贡献 ----
    if let Some((_, _, c)) = conservative.or(results.last()) {
        let promo_final = c
            .equity_curve
            .last()
            .map(|p| p.equity)
            .unwrap_or(initial_equity);
        println!("\n{}", "-".repeat(72));
        println!("费率贡献（多少收益来自零费率活动）");
        println!("{}", "-".repeat(72));
        format::kv(
            "0% maker 费率下",
            &format::signed((promo_final - initial_equity).round_dp(2)),
        );
        format::kv(
            "0.02% maker 费率下",
            &format::signed((c.final_equity_at_standard_fee - initial_equity).round_dp(2)),
        );
        format::kv(
            "活动贡献",
            &format::signed(c.provenance.fees.fee_contribution.round_dp(2)),
        );
        if c.provenance.fees.fee_contribution > (promo_final - initial_equity).abs() {
            println!();
            println!("  ⚠ 收益主要来自零费率活动本身，而非策略的价差捕获能力。");
        }
    }

    // ---- 风控拒绝统计 ----
    if let Some((_, _, c)) = conservative.or(results.last()) {
        if !c.rejections.is_empty() {
            println!("\n{}", "-".repeat(72));
            println!("风控与信号拒绝统计");
            println!("{}", "-".repeat(72));
            let items: Vec<(&str, usize)> = c.rejections.iter().map(|(k, v)| (*k, *v)).collect();
            format::tally("", &items);
        }
    }

    println!("\n{}", "=".repeat(72));
}

/// 找出保守模型的结果（或退回到最后一个）。
fn conservative_of(
    results: &[(String, Box<dyn FillModel>, BacktestResult)],
) -> Option<&(String, Box<dyn FillModel>, BacktestResult)> {
    results
        .iter()
        .find(|(_, m, _)| m.optimism() == Optimism::ConservativeLower)
        .or_else(|| results.last())
}

/// 统计出场方式分布。
fn count_exit_kinds(trades: &[sim::TradeRecord]) -> Vec<(&'static str, usize)> {
    let mut tp = 0;
    let mut sl = 0;
    let mut forced = 0;
    for t in trades {
        match t.exit_reason {
            sim::ExitKind::TakeProfit => tp += 1,
            sim::ExitKind::StopLoss => sl += 1,
            sim::ExitKind::ForcedAtEnd => forced += 1,
        }
    }
    let mut v = Vec::new();
    if tp > 0 {
        v.push(("止盈成交", tp));
    }
    if sl > 0 {
        v.push(("止损成交", sl));
    }
    if forced > 0 {
        v.push(("回测结束强制平仓", forced));
    }
    v
}

/// 打印跨模型裁决。这是反自欺机制最重要的输出。
fn print_verdict(optimistic: &BacktestResult, conservative: &BacktestResult, initial: Decimal) {
    let m = sim::compare_models(optimistic, conservative);

    println!("\n{}", "=".repeat(72));
    println!("结论可信度裁决");
    println!("{}", "=".repeat(72));

    let o_pnl = m.m0_final_equity - initial;
    let c_pnl = m.m1_final_equity - initial;

    println!("  乐观上界（M0）  {}", format::signed(o_pnl.round_dp(2)));
    println!("  诚实下界（M1）  {}", format::signed(c_pnl.round_dp(2)));

    if let Some(r) = m.breakeven_fill_rate {
        println!();
        println!("  盈亏平衡成交率：{}", format::pct(r));
        println!("    含义：M1 需要达到 M0 假设成交量的这个比例，策略才不亏。");
        if r >= Decimal::new(8, 1) {
            println!("    ⚠ 高于 80%——这是成交率赌注，不是做市 edge。");
        }
    } else {
        println!();
        println!("  盈亏平衡成交率：无法计算（乐观模型本身就不盈利）。");
    }

    println!();
    let verdict = m.verdict();
    let marker = if m.is_conclusive() { "✓" } else { "⚠" };
    println!("  {marker} {verdict}");
    println!("{}", "=".repeat(72));
}

/// 从成交模型结果里取出对比用的指标对。供测试使用。
#[allow(dead_code)]
fn pair_for_compare(a: &BacktestResult, b: &BacktestResult) -> EdgeMetrics {
    sim::compare_models(a, b)
}

/// 把日期转成毫秒时间戳（供调试输出）。
#[allow(dead_code)]
fn ts(d: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("合法时间"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn instrument_defaults_to_usdc_contract_rules() {
        let i = instrument_for("ETHUSDC").unwrap();
        assert_eq!(i.settlement_asset, "USDC");
        assert_eq!(i.margin_asset, "USDC");
        assert_eq!(i.kind, ContractKind::CryptoPerp);
        // 真实维持保证金率是 2.5%，不是旧实现硬编码的 0.4%
        assert_eq!(i.maint_margin_pct, dec!(2.5));
    }

    #[test]
    fn tradfi_symbols_settle_in_usdt() {
        let i = instrument_for("XAUUSDT").unwrap();
        assert_eq!(i.settlement_asset, "USDT");
        assert_eq!(i.kind, ContractKind::TradFiPerp);
        assert_eq!(i.base_asset, "XAU");
    }

    /// 零费率活动下 maker 必须是 0，但来源必须标为「假设」——
    /// 这样回测结果才会被标记为不完整。
    #[test]
    fn maker_fee_is_zero_but_flagged_as_assumed() {
        let i = instrument_for("ETHUSDC").unwrap();
        assert_eq!(i.fees.maker_rate, Decimal::ZERO);
        assert_eq!(i.fees.source, FeeSource::PromotionalAssumed);
        assert!(
            !i.fees.source.is_authoritative(),
            "活动费率未经对账，不能被视为权威"
        );
    }

    /// USDC 与 USDT 是两套结算资产，绝不能混。
    #[test]
    fn usdc_and_usdt_instruments_are_distinct() {
        let u = instrument_for("ETHUSDC").unwrap();
        let t = instrument_for("ETHUSDT").unwrap();
        assert_ne!(u.settlement_asset, t.settlement_asset);
        assert_ne!(u.symbol, t.symbol);
    }

    #[test]
    fn ts_converts_date_to_midnight_utc() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let t = ts(d);
        assert_eq!(t.timestamp_millis(), 1_785_542_400_000);
    }
}
