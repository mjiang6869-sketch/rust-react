//! 回测引擎。
//!
//! # 结构
//!
//! 引擎把已经验证过的部件串起来，自身只做时序推进：
//!
//! ```text
//! 行情事件（K线 + 逐笔成交）
//!     ↓
//! Strategy::evaluate       → StrategyIntent（意图，不含价格计算）
//!     ↓
//! risk::check_entry        → 风控裁决（拒绝则记录原因并继续）
//!     ↓
//! ProtectionPlanner::compile → 具体订单（唯一的止盈止损数学）
//!     ↓
//! FillModel::evaluate      → 成交判定（M0 或 M1）
//!     ↓
//! OrderBookState::apply    → 订单状态与持仓的唯一权威
//! ```
//!
//! 引擎**不含任何价格计算**——所有价格来自 `Precision` 与 `ProtectionPlanner`。
//! 这是防止"第四份止盈公式"的结构保证。
//!
//! # 与实盘的关系
//!
//! 这里刻意不引入新的订单生命周期逻辑：实盘的 `LiveAdapter` 会驱动同一个
//! `OrderBookState` 与同一个 `ProtectionPlanner`，只是把 `FillModel::evaluate`
//! 换成交易所的真实回报。所以"回测与实盘不一致"在结构上不可能发生。

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use domain::{
    Candle, ClientOrderId, EntryFill, ExecEvent, Fill, Instrument, MarketEvent, MarketView, Order,
    OrderBookState, OrderPurpose, OrderState, Position, Price, ProtectionAction, ProtectionPlan,
    ProtectionPlanner, Qty, RiskLimits, RiskVerdict, Side, StandDownReason, Strategy,
    StrategyIntent, TpPlan, check_entry, resolve_size,
};
use rust_decimal::Decimal;

use crate::fill::{FillContext, FillModel, Optimism};
use crate::liquidity::TradeTape;
use crate::metrics::{
    BacktestProvenance, EdgeMetrics, FeeHonesty, GapSummary, LatencyStats, MarkoutObservation,
    MarkoutSide, StopExposure, StopResolution,
};

/// 回测配置。
pub struct BacktestConfig {
    pub instrument: Instrument,
    pub limits: RiskLimits,
    pub initial_equity: Decimal,
    /// 参考区间的回看根数（用于策略）。
    pub lookback: usize,
    /// 提交延迟假设（毫秒）。post-only 单在价格已动时会被拒，延迟越大概率越高。
    pub assumed_latency_ms: u64,
    /// 允许跨越数据缺口。默认 false——跨越缺口会凭空发明成交。
    pub allow_gaps: bool,
    /// 费率来源。非权威来源会让结果标记为不完整。
    pub fee_source: domain::FeeSource,
    /// 按常规费率（而非零费率活动）计算时使用的 maker 费率，用于展示
    /// "多少收益来自活动"。
    pub standard_maker_rate: Decimal,
}

/// 一笔已完成的交易记录，用于明细导出与 markout。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradeRecord {
    pub entry_at: DateTime<Utc>,
    pub exit_at: DateTime<Utc>,
    pub side: Side,
    pub quantity: Decimal,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub fee: Decimal,
    /// 出场原因。
    pub exit_reason: ExitKind,
    /// 已实现盈亏（含手续费）。
    pub pnl: Decimal,
}

/// 出场方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitKind {
    TakeProfit,
    StopLoss,
    ForcedAtEnd,
}

/// 权益曲线上的一个点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EquityPoint {
    pub at: DateTime<Utc>,
    pub equity: Decimal,
}

/// 回测结果。
pub struct BacktestResult {
    pub provenance: BacktestProvenance,
    pub metrics: EdgeMetrics,
    pub trades: Vec<TradeRecord>,
    pub equity_curve: Vec<EquityPoint>,
    pub markouts: Vec<MarkoutObservation>,
    pub stop_exposures: Vec<StopExposure>,
    /// 被风控拒绝的次数，按原因统计。
    pub rejections: BTreeMap<&'static str, usize>,
    pub latency: LatencyStats,
    /// 零费率与常规费率两套结果，用于展示"多少收益来自活动"。
    pub final_equity_at_standard_fee: Decimal,
}

/// 引擎内部的一张在途开仓单。
struct PendingEntry {
    order: Order,
    plan: ProtectionPlan,
    tp: TpPlan,
    placed_at: DateTime<Utc>,
    valid_until: DateTime<Utc>,
}

/// 引擎内部的持仓。
struct OpenPosition {
    symbol: String,
    side: Side,
    quantity: Decimal,
    entry_price: Decimal,
    opened_at: DateTime<Utc>,
    plan: ProtectionPlan,
    tp: TpPlan,
    /// 已触发的分批止盈档位数。
    rungs_done: usize,
    /// 自开仓以来的最高/最低成交价，用于移动止损。
    high_since: Decimal,
    low_since: Decimal,
    /// 止损被触发但尚未成交的时刻（用于裸露统计）。
    stop_triggered_at: Option<DateTime<Utc>>,
}

/// 跑一次回测。
///
/// `events` 必须按时间升序（调用方负责，或由 `data` 层保证）。
/// `tape` 是全区间成交带，引擎按需切片。
pub fn run(
    config: &BacktestConfig,
    events: &[MarketEvent],
    tape: &TradeTape,
    strategy: &dyn Strategy,
    fill_model: &dyn FillModel,
) -> BacktestResult {
    let mut equity = config.initial_equity;
    let mut equity_curve = vec![EquityPoint {
        at: events.first().map(|e| e.at()).unwrap_or_else(Utc::now),
        equity,
    }];

    let mut pending: Option<PendingEntry> = None;
    let mut position: Option<OpenPosition> = None;
    let mut trades: Vec<TradeRecord> = Vec::new();
    let mut markouts: Vec<MarkoutObservation> = Vec::new();
    let mut stop_exposures: Vec<StopExposure> = Vec::new();
    let mut rejections: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut latency = LatencyStats {
        attempts: 0,
        rejected_post_only: 0,
        assumed_latency_ms: config.assumed_latency_ms,
    };

    // 累计手续费（按结算资产记账，这里只有单一资产）
    let mut total_fees = Decimal::ZERO;
    let mut standard_fee_total = Decimal::ZERO;

    // 已收盘 K 线滚动窗口
    let mut candles: Vec<Candle> = Vec::with_capacity(config.lookback + 8);
    let mut order_seq: u64 = 0;
    let mut used_signals: Vec<DateTime<Utc>> = Vec::new();

    // 订单状态机。回测里也走它，保证与实盘同一套生命周期。
    let mut state = OrderBookState::new();

    for event in events {
        let now = event.at();

        // ---- 1. 维护 K 线窗口 ----
        if let MarketEvent::Kline(c) = event {
            if c.closed {
                candles.push(c.clone());
                let keep = config.lookback + 2;
                if candles.len() > keep {
                    let drop = candles.len() - keep;
                    candles.drain(0..drop);
                }
            }
        }

        // ---- 2. 更新持仓的极值（用于移动止损）----
        if let MarketEvent::AggTrade(t) = event {
            if let Some(p) = position.as_mut() {
                let px = t.price.get();
                if px > p.high_since {
                    p.high_since = px;
                }
                if px < p.low_since {
                    p.low_since = px;
                }
            }
        }

        // ---- 3. 处理在途开仓单的成交 / 过期 ----
        if let Some(entry) = pending.as_ref() {
            // 过期
            if now >= entry.valid_until {
                pending = None;
            } else {
                let outcome = fill_model.evaluate(&FillContext {
                    order: &entry.order,
                    tape: &TradeTape::from_trades(
                        tape.after(entry.placed_at)
                            .iter()
                            .take_while(|t| t.at <= now)
                            .copied()
                            .collect(),
                    ),
                    placed_at: entry.placed_at,
                    tick_size: config.instrument.precision.tick_size,
                });

                if outcome.is_filled() {
                    latency.attempts += 1;
                    let entry = pending.take().expect("已判存在");
                    let qty = outcome.quantity_for(&entry.order);
                    let px = outcome.price_or(entry.order.limit_price.get());

                    // 走状态机登记与成交
                    let _ = state.register(entry.order.clone(), entry.placed_at);
                    let _ = state.apply(
                        ExecEvent::Accepted {
                            client_id: entry.order.client_id.clone(),
                            exchange_id: format!("bt-{}", entry.order.client_id),
                        },
                        now,
                    );
                    let fee = px * qty * config.instrument.fees.maker_rate;
                    let std_fee = px * qty * config.standard_maker_rate;
                    total_fees += fee;
                    standard_fee_total += std_fee;
                    let _ = state.apply(
                        ExecEvent::Filled(Fill {
                            trade_id: format!("bt-fill-{}", entry.order.client_id),
                            client_id: entry.order.client_id.clone(),
                            quantity: Qty::new(qty),
                            price: Price::new(px),
                            fee,
                            fee_asset: config.instrument.settlement_asset.clone(),
                            at: now,
                        }),
                        now,
                    );
                    equity -= fee;
                    used_signals.push(entry.placed_at);

                    position = Some(OpenPosition {
                        symbol: config.instrument.symbol.clone(),
                        side: entry.order.side,
                        quantity: qty,
                        entry_price: px,
                        opened_at: now,
                        plan: entry.plan.clone(),
                        tp: entry.tp.clone(),
                        rungs_done: 0,
                        high_since: px,
                        low_since: px,
                        stop_triggered_at: None,
                    });
                }
            }
        }

        // ---- 4. 处理持仓的止盈止损 ----
        if let Some(pos) = position.as_ref() {
            let close_side = pos.side.opposite();

            // 4a. 止损判定
            let stop_raw = pos.plan.stop.resolve(pos.entry_price, pos.side);
            let stop_px = config
                .instrument
                .precision
                .price_for(close_side, stop_raw, domain::PriceRole::StopLoss)
                .map(|p| p.get())
                .unwrap_or(stop_raw);

            let stop_hit = match pos.side {
                Side::Buy => pos.low_since <= stop_px,
                Side::Sell => pos.high_since >= stop_px,
            };

            // 4b. 止盈判定（按当前档位）
            let rungs = pos.tp.rungs();
            let rung = rungs.get(pos.rungs_done);
            let tp_hit = rung.map(|(pct, _)| {
                let tp_raw = match pos.side {
                    Side::Buy => pos.entry_price * (Decimal::ONE + pct),
                    Side::Sell => pos.entry_price * (Decimal::ONE - pct),
                };
                config
                    .instrument
                    .precision
                    .price_for(close_side, tp_raw, domain::PriceRole::TakeProfit)
                    .map(|p| p.get())
                    .unwrap_or(tp_raw)
            });

            // 止损优先：同一根 K 线内无法判断先后时取更保守的结果。
            // 这是旧实现 `stop_filled` 先于 `target_filled` 检查的同一个原则。
            let exits: Option<(ExitKind, Decimal)> = if stop_hit {
                Some((ExitKind::StopLoss, stop_px))
            } else if let Some(tp) = tp_hit {
                let reached = match pos.side {
                    Side::Buy => pos.high_since >= tp,
                    Side::Sell => pos.low_since <= tp,
                };
                reached.then_some((ExitKind::TakeProfit, tp))
            } else {
                None
            };

            if let Some((kind, ideal_price)) = exits {
                let pos = position.as_ref().expect("已判存在");
                let close_side = pos.side.opposite();

                // 出场单同样是挂单，走成交模型。这是 maker-only 的关键：
                // 止损可能不成交，仓位继续裸露。
                let exit_order = Order {
                    client_id: ClientOrderId::new("bt-exit", order_seq),
                    symbol: config.instrument.symbol.clone(),
                    purpose: match kind {
                        ExitKind::StopLoss => OrderPurpose::StopLoss,
                        _ => OrderPurpose::TakeProfit,
                    },
                    side: close_side,
                    quantity: Qty::new(pos.quantity),
                    limit_price: Price::new(ideal_price),
                    tif: domain::TimeInForce::PostOnly,
                    parent: None,
                };
                order_seq += 1;

                let outcome = fill_model.evaluate(&FillContext {
                    order: &exit_order,
                    tape: &TradeTape::from_trades(
                        tape.window(pos.opened_at, now + chrono::Duration::seconds(1))
                            .to_vec(),
                    ),
                    placed_at: pos.opened_at,
                    tick_size: config.instrument.precision.tick_size,
                });

                if outcome.is_filled() {
                    let fill_px = outcome.price_or(ideal_price);
                    let qty = outcome.quantity_for(&exit_order);
                    let pos = position.take().expect("已判存在");
                    let fee = fill_px * qty * config.instrument.fees.maker_rate;
                    let std_fee = fill_px * qty * config.standard_maker_rate;
                    total_fees += fee;
                    standard_fee_total += std_fee;

                    let gross = match pos.side {
                        Side::Buy => (fill_px - pos.entry_price) * qty,
                        Side::Sell => (pos.entry_price - fill_px) * qty,
                    };
                    let pnl = gross - fee;
                    equity += gross - fee;

                    trades.push(TradeRecord {
                        entry_at: pos.opened_at,
                        exit_at: now,
                        side: pos.side,
                        quantity: qty,
                        entry_price: pos.entry_price,
                        exit_price: fill_px,
                        fee,
                        exit_reason: kind,
                        pnl,
                    });

                    // markout 观测
                    if let Some(m) = compute_markout(tape, now, fill_px, pos.side) {
                        markouts.push(m);
                    }

                    // 若之前止损曾触发但未成交，记录这段裸露
                    if let Some(triggered) = pos.stop_triggered_at {
                        stop_exposures.push(StopExposure {
                            triggered_at: triggered,
                            resolved_at: now,
                            resolution: StopResolution::FilledLater,
                            resolved_price: fill_px,
                            stop_price: ideal_price,
                            slippage: (fill_px - ideal_price).abs(),
                        });
                    }
                } else {
                    // 未成交：如果是止损则开始记录裸露
                    if kind == ExitKind::StopLoss {
                        let p = position.as_mut().expect("已判存在");
                        if p.stop_triggered_at.is_none() {
                            // 用触发时刻而非当前时刻，避免重复计
                            p.stop_triggered_at = Some(
                                tape.window(p.opened_at, now)
                                    .first()
                                    .map(|t| t.at)
                                    .unwrap_or(now),
                            );
                        }
                    }
                }
            }
        }

        // ---- 5. 无持仓且在途为空时向策略要新意图 ----
        let wants_entry = pending.is_none() && position.is_none();
        if wants_entry {
            let has_pending = false;
            let view = MarketView {
                instrument: &config.instrument,
                candles: &candles,
                now,
                has_position: false,
                has_pending_entry: has_pending,
                equity,
            };

            // 只用 K 线事件触发决策（避免在逐笔上重复计算）
            let trigger = matches!(event, MarketEvent::Kline(c) if c.closed);
            if trigger {
                if let Some(intent) = strategy.evaluate(&view) {
                    match intent {
                        StrategyIntent::StandDown { reason } => {
                            *rejections.entry(reason.message()).or_insert(0) += 1;
                        }
                        StrategyIntent::Enter(req) => {
                            // 信号去重：同一确认时刻只能用一次
                            if used_signals.contains(&now) {
                                *rejections
                                    .entry(StandDownReason::SignalAlreadyUsed.message())
                                    .or_insert(0) += 1;
                            } else {
                                // 风控
                                let entry_px = match config.instrument.precision.price_for(
                                    req.side,
                                    req.entry,
                                    domain::PriceRole::PassiveEntry,
                                ) {
                                    Ok(p) => p.get(),
                                    Err(_) => {
                                        *rejections
                                            .entry(StandDownReason::StopTooWide.message())
                                            .or_insert(0) += 1;
                                        Decimal::ZERO
                                    }
                                };
                                let tp_first = req
                                    .take_profit
                                    .rungs()
                                    .first()
                                    .map(|(pct, _)| match req.side {
                                        Side::Buy => req.entry * (Decimal::ONE + pct),
                                        Side::Sell => req.entry * (Decimal::ONE - pct),
                                    })
                                    .unwrap_or(req.entry);
                                let leverage = match req.size {
                                    domain::SizeHint::EquityFraction { leverage, .. } => leverage,
                                    domain::SizeHint::Fixed(_) => Decimal::ONE,
                                };

                                let verdict = check_entry(
                                    &config.instrument,
                                    req.side,
                                    req.entry,
                                    req.stop,
                                    tp_first,
                                    leverage,
                                    &config.limits,
                                );

                                match verdict {
                                    RiskVerdict::Reject(reason) => {
                                        *rejections.entry(reason.message()).or_insert(0) += 1;
                                    }
                                    RiskVerdict::Pass => {
                                        if entry_px > Decimal::ZERO {
                                            // 提交延迟检查：post-only 在价格已动时被拒。
                                            // 用延迟窗口内的成交判断价格是否已穿过我们的价位。
                                            let delay = chrono::Duration::milliseconds(
                                                config.assumed_latency_ms as i64,
                                            );
                                            let during = tape.window(now, now + delay);
                                            let crossed = during.iter().any(|t| match req.side {
                                                Side::Buy => t.price.get() <= entry_px,
                                                Side::Sell => t.price.get() >= entry_px,
                                            });
                                            latency.attempts += 1;
                                            if crossed {
                                                // 币安错误码 5022：post-only 会立即成交被拒
                                                latency.rejected_post_only += 1;
                                            } else if let Some(qty) = resolve_size(
                                                req.size,
                                                Price::new(entry_px),
                                                equity,
                                                &config.instrument.precision,
                                            ) {
                                                order_seq += 1;
                                                let order = Order {
                                                    client_id: ClientOrderId::new(
                                                        "bt-entry", order_seq,
                                                    ),
                                                    symbol: config.instrument.symbol.clone(),
                                                    purpose: OrderPurpose::Entry,
                                                    side: req.side,
                                                    quantity: qty,
                                                    limit_price: Price::new(entry_px),
                                                    tif: domain::TimeInForce::PostOnly,
                                                    parent: None,
                                                };
                                                pending = Some(PendingEntry {
                                                    order,
                                                    plan: req.protection.clone(),
                                                    tp: req.take_profit.clone(),
                                                    placed_at: now,
                                                    valid_until: req.valid_until,
                                                });
                                            } else {
                                                *rejections
                                                    .entry(
                                                        StandDownReason::InsufficientEquity
                                                            .message(),
                                                    )
                                                    .or_insert(0) += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        StrategyIntent::ExitNow { .. } => {
                            // 回测里 ExitNow 只用于信号失效，直接撤在途单
                            pending = None;
                        }
                    }
                }
            }
        }

        // ---- 6. 权益曲线采样（按小时，避免曲线点过多）----
        if let Some(last) = equity_curve.last() {
            if now - last.at >= chrono::Duration::hours(1) {
                equity_curve.push(EquityPoint { at: now, equity });
            }
        }
    }

    // ---- 收尾：强制平掉未结束的持仓 ----
    if let Some(pos) = position.take() {
        // 强制平仓价用**最后一笔成交价**。
        //
        // 早先的写法用 `high_since`，那是持仓期内的最高价——用它平仓等于假设
        // 我们在最高点出场，是最乐观的估计。回测收尾必须用可观测的市价，
        // 不能用一个对自己有利的极值。
        let last_px = events
            .iter()
            .rev()
            .find_map(|e| match e {
                MarketEvent::AggTrade(t) => Some(t.price.get()),
                _ => None,
            })
            .unwrap_or(pos.entry_price);
        let gross = match pos.side {
            Side::Buy => (last_px - pos.entry_price) * pos.quantity,
            Side::Sell => (pos.entry_price - last_px) * pos.quantity,
        };
        let fee = last_px * pos.quantity * config.instrument.fees.maker_rate;
        let std_fee = last_px * pos.quantity * config.standard_maker_rate;
        total_fees += fee;
        standard_fee_total += std_fee;
        equity += gross - fee;

        let _symbol = pos.symbol.as_str();
        trades.push(TradeRecord {
            entry_at: pos.opened_at,
            exit_at: events.last().map(|e| e.at()).unwrap_or_else(Utc::now),
            side: pos.side,
            quantity: pos.quantity,
            entry_price: pos.entry_price,
            exit_price: last_px,
            fee,
            exit_reason: ExitKind::ForcedAtEnd,
            pnl: gross - fee,
        });

        if let Some(triggered) = pos.stop_triggered_at {
            stop_exposures.push(StopExposure {
                triggered_at: triggered,
                resolved_at: events.last().map(|e| e.at()).unwrap_or_else(Utc::now),
                resolution: StopResolution::StillOpenAtEnd,
                resolved_price: last_px,
                stop_price: pos.plan.stop.resolve(pos.entry_price, pos.side),
                slippage: (last_px - pos.plan.stop.resolve(pos.entry_price, pos.side)).abs(),
            });
        }
    }

    equity_curve.push(EquityPoint {
        at: events.last().map(|e| e.at()).unwrap_or_else(Utc::now),
        equity,
    });

    // 组装结果
    let markout_summary = crate::metrics::summarize_markouts(&markouts);
    let stop_summary = crate::metrics::summarize_stop_exposures(&stop_exposures);

    let standard_final = config.initial_equity + (equity - config.initial_equity)
        - (standard_fee_total - total_fees);

    let metrics = EdgeMetrics {
        // 单次运行只知道一个模型的权益；跨模型对比由调用方填充。
        m0_final_equity: equity,
        m1_final_equity: equity,
        breakeven_fill_rate: None,
        sign_flips: false,
        markout: markout_summary,
        stop_exposure: stop_summary,
        latency: latency.clone(),
    };

    let gaps = GapSummary {
        gap_count: 0,
        total_missing_secs: 0,
        allowed_by_flag: config.allow_gaps,
    };

    let provenance = BacktestProvenance {
        symbol: config.instrument.symbol.clone(),
        strategy_id: strategy.id().to_string(),
        strategy_params: BTreeMap::new(),
        fill_model: fill_model.name().to_string(),
        fill_model_optimism: match fill_model.optimism() {
            Optimism::UpperBound => "上界（不现实）".to_string(),
            Optimism::ConservativeLower => "保守下界".to_string(),
        },
        start: events.first().map(|e| e.at()).unwrap_or_else(Utc::now),
        end: events.last().map(|e| e.at()).unwrap_or_else(Utc::now),
        candle_count: events
            .iter()
            .filter(|e| matches!(e, MarketEvent::Kline(_)))
            .count(),
        trade_count: trades.len(),
        fees: FeeHonesty {
            source: config.fee_source,
            maker_rate_used: config.instrument.fees.maker_rate,
            fee_contribution: standard_fee_total - total_fees,
            incomplete: !config.fee_source.is_authoritative(),
        },
        gaps,
    };

    BacktestResult {
        provenance,
        metrics,
        trades,
        equity_curve,
        markouts,
        stop_exposures,
        rejections,
        latency,
        final_equity_at_standard_fee: standard_final,
    }
}

/// 计算一笔成交的 markout。
///
/// 口径：成交后 +1s/+5s/+30s/+5m 的**参考价**相对成交价的变化，已按持仓
/// 方向取符号（正数 = 对我们有利）。
///
/// 参考价用成交价而非中间价：`bookTicker` 归档已于 2024-04 停更，历史回测
/// **没有**真实盘口。用成交价做参考会略低估噪声，但方向判断是可靠的——而
/// 我们需要的正是方向（是否被逆向选择）。
pub fn compute_markout(
    tape: &TradeTape,
    fill_at: DateTime<Utc>,
    fill_price: Decimal,
    side: Side,
) -> Option<MarkoutObservation> {
    let px_after = |secs: i64| -> Decimal {
        let target = fill_at + chrono::Duration::seconds(secs);
        tape.first_price_after(target)
            .map(|p| p.get())
            .unwrap_or(fill_price)
    };

    let signed = |px: Decimal| -> Decimal {
        match side {
            Side::Buy => px - fill_price,
            Side::Sell => fill_price - px,
        }
    };

    Some(MarkoutObservation {
        at: fill_at,
        price: fill_price,
        side: match side {
            Side::Buy => MarkoutSide::Long,
            Side::Sell => MarkoutSide::Short,
        },
        markout_1s: signed(px_after(1)),
        markout_5s: signed(px_after(5)),
        markout_30s: signed(px_after(30)),
        markout_5m: signed(px_after(300)),
    })
}

/// 跨模型对比，填充 `EdgeMetrics` 里的对比字段。
///
/// 这是反自欺机制的核心：把 M0（乐观上界）与 M1（诚实下界）放在一起，
/// 并算出盈亏平衡成交率。**不允许只查看 M0 的结果。**
pub fn compare_models(optimistic: &BacktestResult, conservative: &BacktestResult) -> EdgeMetrics {
    let m0 = optimistic
        .equity_curve
        .last()
        .map(|p| p.equity)
        .unwrap_or(Decimal::ZERO);
    let m1 = conservative
        .equity_curve
        .last()
        .map(|p| p.equity)
        .unwrap_or(Decimal::ZERO);
    let initial = optimistic
        .equity_curve
        .first()
        .map(|p| p.equity)
        .unwrap_or(Decimal::ZERO);

    let m0_profit = m0 - initial;
    let m1_profit = m1 - initial;
    let sign_flips = (m0_profit > Decimal::ZERO) != (m1_profit > Decimal::ZERO);

    // 盈亏平衡成交率：保守模型需要达到乐观模型利润的多少比例才不亏。
    //
    // 这是线性近似——假设成交量与利润近似成正比，不改变策略行为。
    // 真实关系并非严格线性（成交量变化会改变库存与出场时机），所以这个值
    // 是**量级判断**而非精确阈值：它用来回答"这个结论是不是纯粹依赖成交
    // 假设"，而不是预测精确的盈亏平衡点。
    let breakeven_fill_rate = if m0_profit > Decimal::ZERO {
        // 需要达到乐观利润的多少比例才能覆盖成本：
        //   m1_profit >= 0  时，所需比例 = 1 - m1/m0（已在盈利，还有余量）
        //   m1_profit <  0  时，所需比例 = (m0 - m1)/m0（需要额外成交量补亏）
        let needed = if m1_profit >= Decimal::ZERO {
            let retained = m1_profit / m0_profit;
            Decimal::ONE - retained.min(Decimal::ONE)
        } else {
            (m0_profit - m1_profit) / m0_profit
        };
        Some(needed.max(Decimal::ZERO))
    } else {
        None
    };

    EdgeMetrics {
        m0_final_equity: m0,
        m1_final_equity: m1,
        breakeven_fill_rate,
        sign_flips,
        markout: conservative.metrics.markout.clone(),
        stop_exposure: conservative.metrics.stop_exposure.clone(),
        latency: conservative.latency.clone(),
    }
}

/// 找到持仓在给定时刻的持仓对象（用于外部查询）。
pub fn position_of<'a>(state: &'a OrderBookState, symbol: &str) -> Option<&'a Position> {
    state.position(symbol)
}

/// 订单是否仍开着（供编排层判断）。
pub fn is_order_open(state: &OrderBookState, id: &ClientOrderId) -> bool {
    state.get(id).is_some_and(|t| {
        matches!(
            t.state,
            OrderState::Live | OrderState::PartiallyFilled { .. }
        )
    })
}

/// 把保护单动作转成订单（供模拟盘/实盘复用）。
pub fn compile_protection(
    instrument: &Instrument,
    fill: &EntryFill,
    plan: &ProtectionPlan,
    tp: &TpPlan,
) -> Vec<ProtectionAction> {
    ProtectionPlanner::compile(instrument, fill, plan, tp).unwrap_or_default()
}

/// 参考区间（转发给策略用，保持 domain 的单一实现）。
pub use domain::reference_range as range_of;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fill::{M0WickTouchFull, M1TradeThroughQueue};
    use crate::liquidity::{Trade, TradeTape};
    use domain::{
        ContractKind, FeeSchedule, FeeSource, ParameterSpec, Precision, StopSpec, reference_range,
    };
    use rust_decimal_macros::dec;

    fn instrument() -> Instrument {
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
                observed_at: Utc::now(),
            },
        }
    }

    fn config() -> BacktestConfig {
        BacktestConfig {
            instrument: instrument(),
            limits: RiskLimits {
                max_stop_pct: dec!(0.01),
                min_reward_risk: dec!(1),
                max_feed_staleness_secs: 15,
            },
            initial_equity: dec!(10000),
            lookback: 5,
            assumed_latency_ms: 100,
            allow_gaps: false,
            fee_source: FeeSource::PromotionalAssumed,
            standard_maker_rate: dec!(0.0002),
        }
    }

    /// 测试策略：在区间低点挂买单，固定止盈止损。
    struct RetestLike {
        tp_pct: Decimal,
        stop_pct: Decimal,
    }

    impl Strategy for RetestLike {
        fn id(&self) -> &'static str {
            "retest_like"
        }
        fn name(&self) -> &'static str {
            "测试用回踩策略"
        }
        fn parameters(&self) -> Vec<ParameterSpec> {
            vec![]
        }
        fn warmup_candles(&self) -> usize {
            5
        }
        fn evaluate(&self, view: &MarketView<'_>) -> Option<StrategyIntent> {
            let range = reference_range(view.candles, view.candles.len().min(5))?;
            let entry = range.low;
            let stop = entry * (Decimal::ONE - self.stop_pct);
            if !check_entry(
                view.instrument,
                Side::Buy,
                entry,
                stop,
                entry * (Decimal::ONE + self.tp_pct),
                Decimal::ONE,
                &RiskLimits {
                    max_stop_pct: dec!(0.05),
                    min_reward_risk: dec!(1),
                    max_feed_staleness_secs: 15,
                },
            )
            .is_pass()
            {
                return Some(StrategyIntent::StandDown {
                    reason: StandDownReason::StopTooWide,
                });
            }
            Some(StrategyIntent::Enter(Box::new(domain::EnterRequest {
                side: Side::Buy,
                entry,
                stop,
                take_profit: TpPlan::Single { pct: self.tp_pct },
                protection: ProtectionPlan {
                    stop: StopSpec::Structural { price: stop },
                    break_even: None,
                    trailing: None,
                    timed_cancel: None,
                },
                size: domain::SizeHint::Fixed(dec!(0.1)),
                valid_until: view.now + chrono::Duration::minutes(2),
            })))
        }
    }

    fn t_start() -> i64 {
        1_785_542_400_000
    }

    fn candle(i: i64, low: Decimal, high: Decimal) -> MarketEvent {
        MarketEvent::Kline(Candle {
            open_time: chrono::DateTime::from_timestamp_millis(t_start() + i * 60_000).unwrap(),
            open: low,
            high,
            low,
            close: high,
            volume: dec!(1),
            closed: true,
        })
    }

    fn trade(id: u64, ms_offset: i64, px: Decimal, buyer_maker: bool) -> Trade {
        Trade {
            trade_id: id,
            price: Price::new(px),
            quantity: Qty::new(dec!(1)),
            is_buyer_maker: buyer_maker,
            at: chrono::DateTime::from_timestamp_millis(t_start() + ms_offset).unwrap(),
        }
    }

    /// 最小可用场景：K 线建立区间，随后成交打到区间低点触发成交，
    /// 再涨到止盈价。
    fn scenario() -> (Vec<MarketEvent>, TradeTape) {
        let mut events = Vec::new();
        // 前 6 根 K 线建立区间：low = 3190, high = 3210
        for i in 0..6 {
            events.push(candle(i, dec!(3190), dec!(3210)));
        }
        // 后续 K 线保持区间
        for i in 6..20 {
            events.push(candle(i, dec!(3190), dec!(3210)));
        }

        // 成交：在低点 3190 有卖方主动成交（能吃掉我们的买单）
        let mut trades = vec![
            trade(1, 6 * 60_000 + 100, dec!(3205), false),
            trade(2, 6 * 60_000 + 200, dec!(3190), true), // 打到我们的价位，卖方主动
            trade(3, 6 * 60_000 + 300, dec!(3200), false),
        ];
        // 之后涨到止盈区
        for k in 0..10 {
            trades.push(trade(
                10 + k,
                8 * 60_000 + k as i64 * 1000,
                dec!(3210),
                false, // 买方主动 -> 能吃掉我们的卖单
            ));
        }
        (events, TradeTape::from_trades(trades))
    }

    #[test]
    fn backtest_runs_and_produces_provenance() {
        let (events, tape) = scenario();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };
        let result = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );

        assert_eq!(result.provenance.symbol, "ETHUSDC");
        assert_eq!(result.provenance.strategy_id, "retest_like");
        assert_eq!(result.provenance.fill_model, "M1_trade_through_queue");
        assert!(result.provenance.fill_model_optimism.contains("保守"));
        assert!(!result.equity_curve.is_empty(), "必须有权益曲线");
        assert!(result.provenance.candle_count > 0);
    }

    /// 费率来源非权威时必须标记不完整——整个 edge 依赖零费率活动。
    #[test]
    fn provenance_flags_non_authoritative_fee() {
        let (events, tape) = scenario();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };
        let result = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );
        assert!(
            result.provenance.fees.incomplete,
            "活动费率未经对账时必须标记结果不完整"
        );
    }

    /// 同样数据下 M0（乐观）与 M1（诚实）的成交次数会不同，
    /// 这正是"成交假设影响结论"的机制。
    #[test]
    fn optimistic_model_fills_more_than_conservative() {
        let (events, tape) = scenario();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };

        let m0 = run(&config(), &events, &tape, &strat, &M0WickTouchFull);
        let m1 = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );

        assert!(
            m0.trades.len() >= m1.trades.len(),
            "乐观模型的成交不应少于诚实模型：M0={} M1={}",
            m0.trades.len(),
            m1.trades.len()
        );
    }

    /// 跨模型对比必须能识别符号翻转。
    #[test]
    fn compare_models_detects_sign_flip() {
        let (events, tape) = scenario();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };

        let mut m0 = run(&config(), &events, &tape, &strat, &M0WickTouchFull);
        let mut m1 = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );

        // 构造 M0 盈利而 M1 亏损的情形
        m0.equity_curve.push(EquityPoint {
            at: Utc::now(),
            equity: dec!(10500),
        });
        m1.equity_curve.push(EquityPoint {
            at: Utc::now(),
            equity: dec!(9800),
        });

        let cmp = compare_models(&m0, &m1);
        assert!(cmp.sign_flips, "应识别符号翻转：{cmp:?}");
        assert!(!cmp.is_conclusive());
        assert!(cmp.verdict().contains("不可信"));
    }

    /// 两个模型都盈利且差异不大时，结论应判为稳健。
    #[test]
    fn compare_models_flags_healthy_result_as_conclusive() {
        let (events, tape) = scenario();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };

        let mut m0 = run(&config(), &events, &tape, &strat, &M0WickTouchFull);
        let mut m1 = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );

        // 两者都盈利，M1 保留了大部分收益
        m0.equity_curve.push(EquityPoint {
            at: Utc::now(),
            equity: dec!(11000),
        });
        m1.equity_curve.push(EquityPoint {
            at: Utc::now(),
            equity: dec!(10700),
        });

        let cmp = compare_models(&m0, &m1);
        assert!(!cmp.sign_flips);
        assert!(
            cmp.breakeven_fill_rate.is_some_and(|r| r < dec!(0.5)),
            "应算出较小的盈亏平衡成交率：{:?}",
            cmp.breakeven_fill_rate
        );
    }

    /// 权益曲线必须从初始权益开始——否则收益率的计算基准会错。
    #[test]
    fn equity_curve_starts_at_initial_equity() {
        let (events, tape) = scenario();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };
        let result = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );
        assert_eq!(result.equity_curve[0].equity, dec!(10000));
    }

    /// 零费率活动下，常规费率的权益必须更低——用于展示"多少收益来自活动"。
    #[test]
    fn standard_fee_equity_is_not_better_than_promotional() {
        let (events, tape) = scenario();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };
        let result = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );
        let promo = result.equity_curve.last().unwrap().equity;
        assert!(
            result.final_equity_at_standard_fee <= promo,
            "常规费率下权益不应更高：标准={} 活动={}",
            result.final_equity_at_standard_fee,
            promo
        );
        assert!(
            result.provenance.fees.fee_contribution >= Decimal::ZERO,
            "费率贡献应为非负"
        );
    }

    /// 无任何成交时不应崩溃，且结果仍完整。
    #[test]
    fn empty_tape_does_not_panic() {
        let (events, _) = scenario();
        let tape = TradeTape::default();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };
        let result = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );
        assert_eq!(result.trades.len(), 0, "没有成交就没有交易");
        assert!(result.equity_curve.len() >= 2, "曲线仍有起止点");
    }

    #[test]
    fn empty_events_does_not_panic() {
        let tape = TradeTape::default();
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.004),
        };
        let result = run(
            &config(),
            &[],
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );
        assert_eq!(result.trades.len(), 0);
        assert!(result.provenance.candle_count == 0);
    }

    /// 风控拒绝必须被计数并可展示，不能静默丢弃。
    #[test]
    fn rejections_are_counted_by_reason() {
        // 止损距离远超上限的策略
        let strat = RetestLike {
            tp_pct: dec!(0.003),
            stop_pct: dec!(0.5),
        };
        let (events, tape) = scenario();
        let result = run(
            &config(),
            &events,
            &tape,
            &strat,
            &M1TradeThroughQueue::default(),
        );
        assert!(
            !result.rejections.is_empty(),
            "止损过宽应产生可见的拒绝记录，实际：{:?}",
            result.rejections
        );
        assert!(
            result.rejections.keys().any(|k| k.contains("止损")),
            "拒绝原因应说明是止损问题：{:?}",
            result.rejections
        );
    }

    #[test]
    fn markout_sign_follows_position_direction() {
        let tape = TradeTape::from_trades(vec![
            trade(1, 0, dec!(3200), false),
            trade(2, 10_000, dec!(3210), false), // 10 秒后涨到 3210
        ]);
        let fill_at = chrono::DateTime::from_timestamp_millis(t_start()).unwrap();

        // 多头：价格上涨对我们有利 -> 正
        let long = compute_markout(&tape, fill_at, dec!(3200), Side::Buy).unwrap();
        assert!(
            long.markout_5s > Decimal::ZERO,
            "多头遇涨价应为正：{long:?}"
        );

        // 空头：价格上涨对我们不利 -> 负
        let short = compute_markout(&tape, fill_at, dec!(3200), Side::Sell).unwrap();
        assert!(
            short.markout_5s < Decimal::ZERO,
            "空头遇涨价应为负：{short:?}"
        );
    }

    /// 没有后续成交时 markout 应为 0，而不是用未来数据填一个假值。
    #[test]
    fn markout_without_future_trades_is_zero() {
        let tape = TradeTape::from_trades(vec![trade(1, 0, dec!(3200), false)]);
        let fill_at = chrono::DateTime::from_timestamp_millis(t_start()).unwrap();
        let m = compute_markout(&tape, fill_at, dec!(3200), Side::Buy).unwrap();
        assert_eq!(m.markout_5s, Decimal::ZERO);
    }
}
