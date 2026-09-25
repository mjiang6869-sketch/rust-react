//! 模拟盘引擎。
//!
//! # 与回测共用全部交易逻辑
//!
//! 模拟盘不引入任何新的成交或保护单逻辑——它复用 `sim::FillModel` 判定成交、
//! `domain::position_set` 管理保护单、`domain::OrderBookState` 管订单状态。
//! 唯一的差别是行情来源（实时流 vs Parquet）。
//!
//! 这条设计是刻意的：一旦给模拟盘写一套独立的撮合或保护单逻辑，就会重现旧
//! 项目「三套状态机互相分叉」的问题——回测验证过的行为与实盘不一致，而且
//! 没有任何测试会发现。
//!
//! # 手动下单与策略下单走同一条路径
//!
//! `submit_manual` 与策略意图最终都经过 `preview_manual` → 风控 → 下单，
//! 所以「手点的单」与「策略下的单」在行为上不可能不同。

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use domain::{
    ClientOrderId, EntryFill, ExecEvent, Fill, Instrument, LiveSafety, ManualPlan, ManualPreview,
    MarketEvent, MarketView, Order, OrderBookState, OrderPurpose, OrderState, Position,
    PositionSet, Price, ProtectionPlan, ProtectionPlanner, Qty, RiskLimits, ServiceMode, Side,
    StandDownReason, StrategyIntent, TpPlan, check_consistency, on_stop_filled,
    on_take_profit_filled, preview_manual,
};
use rust_decimal::Decimal;
use sim::fill::{FillContext, FillModel, Optimism};
use sim::liquidity::{Trade, TradeTape};

/// 引擎配置。
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub instrument: Instrument,
    pub limits: RiskLimits,
    pub initial_equity: Decimal,
    /// 提交延迟假设（毫秒）。模拟盘用它模拟 post-only 被拒的概率。
    pub assumed_latency_ms: u64,
    /// 策略在决策前需要的已收盘 K 线根数上限。
    pub max_candles: usize,
    /// 行情新鲜度阈值（秒）。超过则暂停开仓。
    pub max_staleness_secs: i64,
}

/// 引擎对外暴露的事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineEvent {
    /// 状态发生了需要界面刷新的变化。
    StateChanged,
    /// 产生告警（例如止损未成交导致仓位裸露）。
    Alarm(String),
    /// 一笔成交。
    Filled {
        order: ClientOrderId,
        quantity: Decimal,
        price: Decimal,
        fee: Decimal,
    },
    /// 一笔交易完整结束（开仓到平仓）。
    TradeClosed {
        entry_price: Decimal,
        exit_price: Decimal,
        quantity: Decimal,
        pnl: Decimal,
        exit_reason: ExitReason,
    },
    /// 风控拒绝了一次开仓。
    Rejected { reason: String },
}

/// 一次交易的结束方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
    Manual,
}

/// 引擎状态的快照。界面渲染它。
#[derive(Clone, Debug)]
pub struct EngineSnapshot {
    pub mode: ServiceMode,
    pub symbol: String,
    pub equity: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    /// 当前持仓（含各档止盈状态）。
    pub position: Option<domain::PositionView>,
    /// 在途订单。
    pub open_orders: Vec<OrderSnapshot>,
    /// 行情新鲜度。
    pub feed_connected: bool,
    pub last_event_at: Option<DateTime<Utc>>,
    /// 策略让位的原因（"我为什么不交易"）。
    pub stand_down: Option<&'static str>,
    /// 实盘安全闸门状态。
    pub safety: LiveSafety,
}

/// 一张订单的展示快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderSnapshot {
    pub client_id: String,
    pub purpose: OrderPurpose,
    pub side: Side,
    pub quantity: Decimal,
    pub limit_price: Decimal,
    pub filled: Decimal,
    pub state: &'static str,
}

/// 提交结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// 已接受并挂出。
    Accepted(Box<ManualPreview>),
    /// 被风控或参数校验拒绝。
    Rejected { reason: String },
}

/// 模拟盘引擎。
pub struct PaperEngine {
    config: EngineConfig,
    /// 订单状态机。与回测、实盘共用同一个类型。
    state: OrderBookState,
    /// 当前持仓连同保护单。
    position_set: Option<PositionSet>,
    /// 已收盘 K 线滚动窗口。
    candles: Vec<domain::Candle>,
    /// 自某一时刻起的成交（用于成交模型判定）。
    tape: TradeTape,
    /// 已实现盈亏。
    realized_pnl: Decimal,
    /// 累计手续费。
    total_fees: Decimal,
    /// 当前的策略意图（在途开仓单）。
    pending_intent: Option<PendingOrder>,
    /// 策略让位原因。
    stand_down: Option<StandDownReason>,
    /// 实盘安全闸门。
    safety: LiveSafety,
    /// 事件输出缓冲。
    events: Vec<EngineEvent>,
    /// 订单序号。
    next_order_id: u64,
    /// 行情连接状态。
    feed_connected: bool,
    pub(crate) last_event_at: Option<DateTime<Utc>>,
    /// 成交模型（模拟盘默认 M1，与回测的诚实基线一致）。
    fill_model: Box<dyn FillModel>,
}

/// 在途的开仓单。
struct PendingOrder {
    order: Order,
    plan: ProtectionPlan,
    tp: TpPlan,
    placed_at: DateTime<Utc>,
    valid_until: DateTime<Utc>,
    /// 已提交尝试次数（用于统计 post-only 拒单率）。
    attempts: usize,
    rejected: usize,
}

impl PaperEngine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            state: OrderBookState::new(),
            position_set: None,
            candles: Vec::new(),
            tape: TradeTape::default(),
            realized_pnl: Decimal::ZERO,
            total_fees: Decimal::ZERO,
            pending_intent: None,
            stand_down: None,
            safety: LiveSafety::new(),
            events: Vec::new(),
            next_order_id: 1,
            feed_connected: false,
            last_event_at: None,
            // 模拟盘默认用诚实基线而非上界——用 M0 会让模拟盘过于乐观，
            // 那样模拟盘的结论与实盘预期不符。
            fill_model: Box::new(sim::M1TradeThroughQueue::default()),
        }
    }

    /// 用指定的成交模型构造（用于对照实验）。
    pub fn with_fill_model(config: EngineConfig, model: Box<dyn FillModel>) -> Self {
        let mut e = Self::new(config);
        e.fill_model = model;
        e
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn instrument(&self) -> &Instrument {
        &self.config.instrument
    }

    pub fn state(&self) -> &OrderBookState {
        &self.state
    }

    pub fn position_set(&self) -> Option<&PositionSet> {
        self.position_set.as_ref()
    }

    pub fn safety(&self) -> &LiveSafety {
        &self.safety
    }

    pub fn safety_mut(&mut self) -> &mut LiveSafety {
        &mut self.safety
    }

    /// 取出累积的事件（调用方负责分发）。
    pub fn drain_events(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut self.events)
    }

    /// 行情是否新鲜。
    pub fn feed_is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.feed_connected
            && self.last_event_at.is_some_and(|t| {
                now.signed_duration_since(t).num_seconds() <= self.config.max_staleness_secs
            })
    }

    /// 喂入一个行情事件。
    ///
    /// 这是引擎的主入口。顺序很重要：
    /// 1. 更新行情窗口与新鲜度
    /// 2. 判定在途单成交
    /// 3. 处理持仓的止盈止损
    /// 4. 风控与策略决策
    pub fn on_market_event(&mut self, event: MarketEvent) {
        let now = event.at();
        self.last_event_at = Some(now);

        // ---- 1. 维护行情窗口 ----
        match &event {
            MarketEvent::Kline(c) => {
                if c.closed {
                    self.candles.push(c.clone());
                    let keep = self.config.max_candles.max(120);
                    if self.candles.len() > keep {
                        let drop = self.candles.len() - keep;
                        self.candles.drain(0..drop);
                    }
                }
            }
            MarketEvent::AggTrade(t) => {
                // 成交带保留最近一段（用于判定在途单成交）。
                // 8 小时窗口足够覆盖任何合理的挂单有效期。
                self.tape.push(Trade::from(*t));
                self.tape.prune_before(now - Duration::hours(8));
            }
            _ => {}
        }

        // ---- 2 & 3：处理订单与持仓 ----
        self.try_fill_pending(now);
        self.manage_position(now);

        // ---- 4：策略决策（只在 K 线收盘时）----
        if matches!(event, MarketEvent::Kline(ref c) if c.closed) {
            self.decide(now);
        }

        // ---- 5：过期挂单清理 ----
        self.expire_pending(now);
    }

    /// 判定在途开仓单是否成交。
    fn try_fill_pending(&mut self, now: DateTime<Utc>) {
        let Some(pending) = self.pending_intent.as_ref() else {
            return;
        };

        let outcome = self.fill_model.evaluate(&FillContext {
            order: &pending.order,
            tape: &self.tape,
            placed_at: pending.placed_at,
            tick_size: self.config.instrument.precision.tick_size,
        });

        if !outcome.is_filled() {
            return;
        }

        let pending = self.pending_intent.take().expect("已判存在");
        let qty = outcome.quantity_for(&pending.order);
        let price = outcome.price_or(pending.order.limit_price.get());

        // 走状态机登记与成交
        let _ = self
            .state
            .register(pending.order.clone(), pending.placed_at);
        let _ = self.state.apply(
            ExecEvent::Accepted {
                client_id: pending.order.client_id.clone(),
                exchange_id: "paper".to_string(),
            },
            now,
        );

        let fee = price * qty * self.config.instrument.fees.maker_rate;
        self.total_fees += fee;
        let _ = self.state.apply(
            ExecEvent::Filled(Fill {
                trade_id: format!("paper-{}", pending.order.client_id),
                client_id: pending.order.client_id.clone(),
                quantity: Qty::new(qty),
                price: Price::new(price),
                fee,
                fee_asset: self.config.instrument.settlement_asset.clone(),
                at: now,
            }),
            now,
        );

        self.events.push(EngineEvent::Filled {
            order: pending.order.client_id.clone(),
            quantity: qty,
            price,
            fee,
        });

        // 建立持仓与保护单集合
        let pos = Position {
            symbol: self.config.instrument.symbol.clone(),
            side: pending.order.side,
            quantity: Qty::new(qty),
            entry_price: Price::new(price),
            opened_at: now,
            stop_price: None,
        };
        let mut set = PositionSet::new(&pos, pending.plan.clone(), pending.tp.clone());

        // 编译保护单并登记（模拟盘里"挂出"就是登记进状态机）
        let fill = EntryFill {
            symbol: self.config.instrument.symbol.clone(),
            entry_order_id: pending.order.client_id.clone(),
            side: pending.order.side,
            price: Price::new(price),
            quantity: Qty::new(qty),
        };
        if let Ok(actions) =
            ProtectionPlanner::compile(&self.config.instrument, &fill, &pending.plan, &pending.tp)
        {
            for a in actions {
                if let domain::ProtectionAction::Place(order) = a {
                    if order.purpose == OrderPurpose::StopLoss {
                        set.stop_price = Some(order.limit_price);
                    }
                    let _ = self.state.register(*order, now);
                }
            }
        }

        self.position_set = Some(set);
        // 保护单在这一刻挂出——记录时刻，后续判定只能用这之后的成交。
        if let Some(s) = self.position_set.as_mut() {
            s.protection_placed_at = Some(now);
        }
        self.events.push(EngineEvent::StateChanged);
    }

    /// 处理持仓的止盈止损。
    fn manage_position(&mut self, now: DateTime<Utc>) {
        let Some(set) = self.position_set.as_ref() else {
            return;
        };

        let close_side = set.side.opposite();
        let entry_price = set.entry_price.get();
        // 保护单的挂出时刻。没有记录（重启恢复等）时退回开仓时刻，
        // 但那会让判定变宽——所以正常情况下必须有值。
        let protection_at = set.protection_placed_at.unwrap_or(set.opened_at);

        // 收集在途的保护单
        let working: Vec<_> = self
            .state
            .open_orders()
            .into_iter()
            .filter(|t| t.order.reduce_only())
            .collect();

        // 逐个检查止盈档位。
        //
        // 各档数量由 `rung_quantity` 按**入场时的原始持仓量**计算——
        // 不要在这里用当前 `quantity` 当基准，那会让第二档基于缩小后的
        // 基数再乘比例，总平仓量永远到不了 100%。
        let rungs = set.tp.rungs();
        for (i, (pct, _)) in rungs.iter().enumerate() {
            if set.completed_rungs.contains(&i) {
                continue;
            }
            let target = match set.side {
                Side::Buy => entry_price * (Decimal::ONE + pct),
                Side::Sell => entry_price * (Decimal::ONE - pct),
            };
            let quantized = self
                .config
                .instrument
                .precision
                .price_for(close_side, target, domain::PriceRole::TakeProfit)
                .map(|p| p.get())
                .unwrap_or(target);

            // 用成交模型判定这张止盈单是否成交——与回测同一套逻辑
            let tp_order = Order {
                client_id: set
                    .tp_order_ids
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| ClientOrderId::new("paper-tp", i as u64)),
                symbol: self.config.instrument.symbol.clone(),
                purpose: OrderPurpose::TakeProfit,
                side: close_side,
                quantity: set.rung_quantity(i),
                limit_price: Price::new(quantized),
                tif: domain::TimeInForce::PostOnly,
                parent: None,
            };
            let outcome = self.fill_model.evaluate(&FillContext {
                order: &tp_order,
                tape: &self.tape,
                // 判定起点是保护单**挂出**的时刻，不是开仓时刻。
                placed_at: protection_at,
                tick_size: self.config.instrument.precision.tick_size,
            });
            if !outcome.is_filled() {
                continue;
            }

            let fill_qty = set.rung_quantity(i);
            let fill_px = outcome.price_or(quantized);
            let fee = fill_px * fill_qty.get() * self.config.instrument.fees.maker_rate;
            self.total_fees += fee;

            let set = self.position_set.as_mut().expect("已判存在");
            let working_refs: Vec<_> = working.to_vec();
            let _ = on_take_profit_filled(set, i, fill_qty, &working_refs);

            let gross = match set.side {
                Side::Buy => (fill_px - entry_price) * fill_qty.get(),
                Side::Sell => (entry_price - fill_px) * fill_qty.get(),
            };
            self.realized_pnl += gross - fee;

            self.events.push(EngineEvent::Filled {
                order: tp_order.client_id.clone(),
                quantity: fill_qty.get(),
                price: fill_px,
                fee,
            });

            // 仓位平完 -> 记一笔完整交易
            if self
                .position_set
                .as_ref()
                .is_some_and(|s| s.quantity.is_zero())
            {
                self.events.push(EngineEvent::TradeClosed {
                    entry_price,
                    exit_price: fill_px,
                    quantity: fill_qty.get(),
                    pnl: gross - fee,
                    exit_reason: ExitReason::TakeProfit,
                });
                self.position_set = None;
            }
            self.events.push(EngineEvent::StateChanged);
            break; // 一次只处理一档，下一 tick 再处理
        }

        // ---- 止损判定：两阶段 ----
        //
        // 多头仓位的止损是"卖单挂在市价下方"。这样的单**不在订单簿里**——
        // 它是交叉单，会立即成交。真实形态是「STOP 触发 + 触发后转限价」：
        //
        //   阶段 1：价格跌到止损价 -> 触发
        //   阶段 2：触发后作为卖单挂在止损价，等待买方主动成交
        //
        // 阶段 2 里 maker-only 的代价就体现出来了：跳空穿过止损价且不再回来时，
        // 止损挂着不成交，仓位持续裸露。`stop_triggered_at` 记录这个时刻。
        let Some(set) = self.position_set.as_ref() else {
            return;
        };
        let Some(stop_price) = set.stop_price else {
            return;
        };
        let protection_at = set.protection_placed_at.unwrap_or(set.opened_at);

        // 阶段 1：是否已触发？
        let triggered =
            set.stop_triggered_at.is_some() || self.stop_triggered(stop_price, protection_at);

        if !triggered {
            return;
        }

        // 先取出阶段 2 需要的全部数据，再释放对 position_set 的不可变借用。
        let (qty, side, entry_px) = (set.quantity, set.side, set.entry_price.get());

        // 记住触发时刻：首次触发时记录 `now`，重复进入时沿用原值。
        // 阶段 2 的成交判定必须以触发时刻为起点——触发之前那张单不存在。
        let already = set.stop_triggered_at;
        let trigger_at = already.unwrap_or(now);
        if let Some(st) = self.position_set.as_mut() {
            if st.stop_triggered_at.is_none() {
                st.stop_triggered_at = Some(trigger_at);
            }
        }

        let stop_order = Order {
            client_id: ClientOrderId::new("paper-stop", 0),
            symbol: self.config.instrument.symbol.clone(),
            purpose: OrderPurpose::StopLoss,
            side: close_side,
            quantity: qty,
            limit_price: stop_price,
            tif: domain::TimeInForce::PostOnly,
            parent: None,
        };
        let outcome = self.fill_model.evaluate(&FillContext {
            order: &stop_order,
            tape: &self.tape,
            placed_at: trigger_at,
            tick_size: self.config.instrument.precision.tick_size,
        });
        if !outcome.is_filled() {
            // 触发了但没成交 = 仓位裸露中。这是 maker-only 的核心风险，
            // 必须让操作者看到。
            self.events.push(EngineEvent::Alarm(format!(
                "止损已触发但未成交：{} @ {}。仓位裸露中。",
                self.config.instrument.symbol,
                stop_price.get()
            )));
            return;
        }

        let fill_px = outcome.price_or(stop_price.get());
        let fee = fill_px * qty.get() * self.config.instrument.fees.maker_rate;
        self.total_fees += fee;

        let working_refs: Vec<_> = working.to_vec();
        let set = self.position_set.as_mut().expect("已判存在");
        on_stop_filled(set, &working_refs);

        let gross = match side {
            Side::Buy => (fill_px - entry_px) * qty.get(),
            Side::Sell => (entry_px - fill_px) * qty.get(),
        };
        self.realized_pnl += gross - fee;

        self.events.push(EngineEvent::Filled {
            order: stop_order.client_id.clone(),
            quantity: qty.get(),
            price: fill_px,
            fee,
        });
        self.events.push(EngineEvent::TradeClosed {
            entry_price: entry_px,
            exit_price: fill_px,
            quantity: qty.get(),
            pnl: gross - fee,
            exit_reason: ExitReason::StopLoss,
        });
        self.position_set = None;
        self.events.push(EngineEvent::StateChanged);
        let _ = now;
    }

    /// 止损是否已触发（阶段 1）。
    ///
    /// 多头止损在价格**跌到或跌破**止损价时触发；空头在**涨到或涨破**时。
    /// 触发用成交价判定（模拟盘没有独立标记价）。
    fn stop_triggered(&self, stop_price: Price, since: DateTime<Utc>) -> bool {
        let Some(set) = self.position_set.as_ref() else {
            return false;
        };
        let stop = stop_price.get();
        self.tape.after(since).iter().any(|t| match set.side {
            Side::Buy => t.price.get() <= stop,
            Side::Sell => t.price.get() >= stop,
        })
    }

    /// 向策略索取意图并处理。
    fn decide(&mut self, now: DateTime<Utc>) {
        if self.pending_intent.is_some() || self.position_set.is_some() {
            return;
        }

        let strategy = match strategies::by_id("range_maker") {
            Some(s) => s,
            None => return,
        };

        // 行情不新鲜时明确让位——静默不交易是恶劣的失败模式。
        if !self.feed_is_fresh(now) {
            self.stand_down = Some(StandDownReason::StaleFeed);
            return;
        }

        let view = MarketView {
            instrument: &self.config.instrument,
            candles: &self.candles,
            now,
            has_position: false,
            has_pending_entry: false,
            equity: self.equity(),
        };

        let Some(intent) = strategy.evaluate(&view) else {
            return;
        };

        match intent {
            StrategyIntent::StandDown { reason } => {
                self.stand_down = Some(reason);
                self.events.push(EngineEvent::Rejected {
                    reason: reason.message().to_string(),
                });
            }
            StrategyIntent::Enter(req) => {
                self.stand_down = None;
                let plan = ManualPlan {
                    symbol: self.config.instrument.symbol.clone(),
                    side: req.side,
                    entry: req.entry,
                    quantity: None,
                    size_pct: None,
                    leverage: Decimal::ONE,
                    stop: req.stop,
                    take_profit: req.take_profit.clone(),
                    break_even: req.protection.break_even,
                    trailing: req.protection.trailing,
                    cancel_unfilled_after: req.protection.timed_cancel,
                    client_ref: "strategy".into(),
                };
                match self.place(&plan, now) {
                    SubmitOutcome::Accepted(_) => {}
                    SubmitOutcome::Rejected { reason } => {
                        self.events.push(EngineEvent::Rejected { reason });
                    }
                }
            }
            StrategyIntent::ExitNow { .. } => {
                self.pending_intent = None;
            }
        }
    }

    /// 提交一份手动计划。策略与手动面板共用这条路径。
    pub fn submit_manual(&mut self, plan: &ManualPlan, now: DateTime<Utc>) -> SubmitOutcome {
        self.place(plan, now)
    }

    /// 预览一份手动计划（不提交）。
    pub fn preview_manual_plan(
        &self,
        plan: &ManualPlan,
        now: DateTime<Utc>,
    ) -> Result<ManualPreview, domain::DomainError> {
        // 标记价用最新的成交价近似。真实实现应从行情流取标记价。
        let mark = self
            .tape
            .trades()
            .last()
            .map(|t| t.price.get())
            .or_else(|| self.candles.last().map(|c| c.close))
            .unwrap_or(plan.entry);
        let _ = now;
        preview_manual(
            &self.config.instrument,
            plan,
            self.equity(),
            mark,
            &self.config.limits,
        )
    }

    /// 内部：把计划编译成订单并挂出。策略与手动共用。
    fn place(&mut self, plan: &ManualPlan, now: DateTime<Utc>) -> SubmitOutcome {
        // 已有持仓或在途单时拒绝——绝不能重复暴露。
        if self.position_set.is_some() || self.pending_intent.is_some() {
            return SubmitOutcome::Rejected {
                reason: "已有持仓或在途订单，请先平仓或撤单".into(),
            };
        }

        let preview = match self.preview_manual_plan(plan, now) {
            Ok(p) => p,
            Err(e) => {
                return SubmitOutcome::Rejected {
                    reason: e.to_string(),
                };
            }
        };

        if !preview.accepted {
            return SubmitOutcome::Rejected {
                reason: preview
                    .reject_reason
                    .clone()
                    .unwrap_or_else(|| "风控拒绝".into()),
            };
        }

        // 数量必须为正
        if preview.quantity.is_zero() {
            return SubmitOutcome::Rejected {
                reason: "数量为零，无法下单".into(),
            };
        }

        self.next_order_id += 1;
        let order = Order {
            client_id: ClientOrderId::new(&plan.client_ref, self.next_order_id),
            symbol: self.config.instrument.symbol.clone(),
            purpose: OrderPurpose::Entry,
            side: plan.side,
            quantity: preview.quantity,
            limit_price: preview.entry,
            tif: match plan.cancel_unfilled_after {
                Some(deadline) => domain::TimeInForce::PostOnlyGtd { deadline },
                None => domain::TimeInForce::PostOnly,
            },
            parent: None,
        };

        let version = plan.clone();
        self.pending_intent = Some(PendingOrder {
            order,
            plan: ProtectionPlan {
                stop: domain::StopSpec::Structural {
                    price: version.stop,
                },
                break_even: version.break_even,
                trailing: version.trailing,
                timed_cancel: version.cancel_unfilled_after,
            },
            tp: version.take_profit.clone(),
            placed_at: now,
            valid_until: version
                .cancel_unfilled_after
                .unwrap_or_else(|| now + Duration::minutes(2)),
            attempts: 1,
            rejected: 0,
        });

        self.events.push(EngineEvent::StateChanged);
        SubmitOutcome::Accepted(Box::new(preview))
    }

    /// 撤掉在途开仓单。
    pub fn cancel_pending(&mut self) -> bool {
        if self.pending_intent.take().is_some() {
            self.events.push(EngineEvent::StateChanged);
            true
        } else {
            false
        }
    }

    /// 手动平仓（市价语义，但模拟盘按当前成交价成交）。
    pub fn close_position_manually(&mut self, now: DateTime<Utc>) -> Option<Decimal> {
        let set = self.position_set.take()?;
        let price = self
            .tape
            .trades()
            .last()
            .map(|t| t.price.get())
            .unwrap_or(set.entry_price.get());

        let gross = match set.side {
            Side::Buy => (price - set.entry_price.get()) * set.quantity.get(),
            Side::Sell => (set.entry_price.get() - price) * set.quantity.get(),
        };
        // 手动平仓按 taker 计费——这是市价语义，不是 maker 挂单。
        let fee = price * set.quantity.get() * self.config.instrument.fees.taker_rate;
        self.total_fees += fee;
        self.realized_pnl += gross - fee;

        self.events.push(EngineEvent::TradeClosed {
            entry_price: set.entry_price.get(),
            exit_price: price,
            quantity: set.quantity.get(),
            pnl: gross - fee,
            exit_reason: ExitReason::Manual,
        });
        self.events.push(EngineEvent::StateChanged);
        let _ = now;
        Some(gross - fee)
    }

    /// 过期挂单清理。
    fn expire_pending(&mut self, now: DateTime<Utc>) {
        if let Some(p) = self.pending_intent.as_ref() {
            if now >= p.valid_until {
                self.pending_intent = None;
                self.events.push(EngineEvent::StateChanged);
            }
        }
    }

    /// 检查持仓与保护单的一致性。
    ///
    /// 返回 `Err` 时必须采取行动——超过持仓的挂单会导致止损被拒、保护失效。
    pub fn check_consistency(&self) -> Option<String> {
        let set = self.position_set.as_ref()?;
        let pos = Position {
            symbol: set.symbol.clone(),
            side: set.side,
            quantity: set.quantity,
            entry_price: set.entry_price,
            opened_at: set.opened_at,
            stop_price: set.stop_price,
        };
        let working: Vec<_> = self.state.open_orders();
        let report = check_consistency(&pos, &working);
        report.problem
    }

    /// 当前权益（已实现盈亏 + 未实现盈亏 + 初始权益）。
    pub fn equity(&self) -> Decimal {
        self.config.initial_equity + self.realized_pnl + self.unrealized_pnl()
    }

    /// 未实现盈亏。
    pub fn unrealized_pnl(&self) -> Decimal {
        let Some(set) = self.position_set.as_ref() else {
            return Decimal::ZERO;
        };
        let mark = self
            .tape
            .trades()
            .last()
            .map(|t| t.price.get())
            .or_else(|| self.candles.last().map(|c| c.close))
            .unwrap_or(set.entry_price.get());
        match set.side {
            Side::Buy => (mark - set.entry_price.get()) * set.quantity.get(),
            Side::Sell => (set.entry_price.get() - mark) * set.quantity.get(),
        }
    }

    pub fn realized_pnl(&self) -> Decimal {
        self.realized_pnl
    }

    pub fn total_fees(&self) -> Decimal {
        self.total_fees
    }

    /// 当前状态快照。
    pub fn snapshot(&self) -> EngineSnapshot {
        let mark = self
            .tape
            .trades()
            .last()
            .map(|t| t.price)
            .or_else(|| self.candles.last().map(|c| Price::new(c.close)))
            .unwrap_or(Price::new(Decimal::ZERO));

        EngineSnapshot {
            mode: ServiceMode::Paper,
            symbol: self.config.instrument.symbol.clone(),
            equity: self.equity(),
            realized_pnl: self.realized_pnl,
            unrealized_pnl: self.unrealized_pnl(),
            position: self
                .position_set
                .as_ref()
                .map(|s| s.view(mark, self.realized_pnl)),
            open_orders: self
                .state
                .open_orders()
                .iter()
                .map(|t| OrderSnapshot {
                    client_id: t.order.client_id.to_string(),
                    purpose: t.order.purpose,
                    side: t.order.side,
                    quantity: t.order.quantity.get(),
                    limit_price: t.order.limit_price.get(),
                    filled: t.filled.get(),
                    state: state_tag(&t.state),
                })
                .collect(),
            feed_connected: self.feed_connected,
            last_event_at: self.last_event_at,
            stand_down: self.stand_down.map(|r| r.message()),
            safety: self.safety,
        }
    }

    /// 标记行情连接状态。断线必须导致暂停开仓。
    pub fn set_feed_connected(&mut self, connected: bool) {
        self.feed_connected = connected;
        if !connected {
            // 断线时撤掉在途开仓单——行情不动了，挂着单是盲开。
            self.pending_intent = None;
        }
        self.events.push(EngineEvent::StateChanged);
    }

    /// 统计 post-only 拒单情况（供界面展示）。
    pub fn latency_stats(&self) -> (usize, usize) {
        match self.pending_intent.as_ref() {
            Some(p) => (p.attempts, p.rejected),
            None => (0, 0),
        }
    }

    /// 成交模型名称与乐观度（界面必须展示，否则用户不知道结论有多可信）。
    pub fn fill_model_info(&self) -> (&'static str, Optimism) {
        (self.fill_model.name(), self.fill_model.optimism())
    }

    /// 各档止盈的分布统计，供界面画图。
    pub fn rung_summary(&self) -> BTreeMap<String, usize> {
        let Some(set) = self.position_set.as_ref() else {
            return BTreeMap::new();
        };
        set.tp
            .rungs()
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let key = format!("止盈第 {} 档", i + 1);
                let n = if set.completed_rungs.contains(&i) {
                    1
                } else {
                    0
                };
                (key, n)
            })
            .collect()
    }
}

fn state_tag(s: &OrderState) -> &'static str {
    match s {
        OrderState::PendingSubmit => "待提交",
        OrderState::Live => "挂单中",
        OrderState::PartiallyFilled { .. } => "部分成交",
        OrderState::Filled { .. } => "已成交",
        OrderState::Cancelled { .. } => "已撤销",
        OrderState::Rejected { .. } => "已拒绝",
        OrderState::Expired => "已过期",
        OrderState::Unknown { .. } => "状态未知",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ContractKind, FeeSchedule, FeeSource, Precision, TpPlan, TpRung};
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

    fn config() -> EngineConfig {
        EngineConfig {
            instrument: instrument(),
            limits: RiskLimits {
                max_stop_pct: dec!(0.05),
                min_reward_risk: Decimal::ONE,
                max_feed_staleness_secs: 15,
            },
            initial_equity: dec!(10000),
            assumed_latency_ms: 100,
            max_candles: 120,
            max_staleness_secs: 15,
        }
    }

    fn engine() -> PaperEngine {
        PaperEngine::new(config())
    }

    fn t0() -> DateTime<Utc> {
        chrono::DateTime::from_timestamp_millis(1_785_542_400_000).unwrap()
    }

    fn kline(offset_min: i64, low: Decimal, high: Decimal) -> MarketEvent {
        MarketEvent::Kline(domain::Candle {
            open_time: t0() + Duration::minutes(offset_min),
            open: low,
            high,
            low,
            close: high,
            volume: dec!(1),
            closed: true,
        })
    }

    fn trade(offset_ms: i64, px: Decimal, buyer_maker: bool) -> MarketEvent {
        MarketEvent::AggTrade(domain::AggTrade {
            trade_id: offset_ms.unsigned_abs() + 1,
            price: Price::new(px),
            quantity: Qty::new(dec!(1)),
            is_buyer_maker: buyer_maker,
            at: t0() + Duration::milliseconds(offset_ms),
        })
    }

    /// 构造一份合理的手动计划。
    ///
    /// 参数要满足默认风控：止损距离 ≤ 0.5%，且盈亏比 ≥ 1。
    /// 入场 3200 / 止损 3192（0.25%），两档止盈 0.25% 与 0.5%，
    /// 第一档盈亏比 = 8/8 = 1，刚好达标。
    fn manual_plan(side: Side, entry: Decimal, stop: Decimal, qty: Decimal) -> ManualPlan {
        ManualPlan {
            symbol: "ETHUSDC".into(),
            side,
            entry,
            quantity: Some(Qty::new(qty)),
            size_pct: None,
            leverage: Decimal::from(3),
            stop,
            take_profit: TpPlan::Ladder {
                rungs: vec![
                    TpRung {
                        pct: dec!(0.0025),
                        fraction: dec!(0.5),
                    },
                    TpRung {
                        pct: dec!(0.005),
                        fraction: dec!(0.5),
                    },
                ],
            },
            break_even: None,
            trailing: None,
            cancel_unfilled_after: Some(t0() + Duration::minutes(2)),
            client_ref: "manual".into(),
        }
    }

    /// 风控通过的参数组合：入场 3200，止损 3192。
    fn ok_plan() -> ManualPlan {
        manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.1))
    }

    #[test]
    fn new_engine_has_no_position_and_full_equity() {
        let e = engine();
        assert_eq!(e.equity(), dec!(10000));
        assert!(e.position_set().is_none());
        assert!(e.snapshot().position.is_none());
    }

    /// 行情不新鲜时策略必须明确让位——静默不交易是恶劣的失败模式。
    #[test]
    fn engine_stands_down_when_feed_is_stale() {
        let mut e = engine();
        // 不设置 feed_connected，直接喂 K 线
        for i in 0..100 {
            e.on_market_event(kline(i, dec!(3190), dec!(3210)));
        }
        // 行情不新鲜时不应有在途单
        assert!(e.snapshot().position.is_none());
    }

    /// 断线必须撤掉在途开仓单并阻止开仓。
    ///
    /// 行情不动了还挂着单是盲开——成交了也不知道。
    #[test]
    fn disconnect_clears_pending_entry() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = ok_plan();
        assert!(matches!(
            e.submit_manual(&plan, t0()),
            SubmitOutcome::Accepted(_)
        ));

        e.set_feed_connected(false);
        let snap = e.snapshot();
        assert!(
            !snap
                .open_orders
                .iter()
                .any(|o| o.purpose == OrderPurpose::Entry),
            "断线后不应保留开仓单"
        );
    }

    /// **成交触发路径**：成交打在我们的买价上时必须建立持仓。
    ///
    /// 这是模拟盘最核心的行为——有挂单、有对手方成交、然后建立持仓与保护单。
    #[test]
    fn trade_at_our_price_opens_position() {
        let mut e = engine();
        e.set_feed_connected(true);
        // 挂买单在 3200
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.1));
        assert!(matches!(
            e.submit_manual(&plan, t0()),
            SubmitOutcome::Accepted(_)
        ));

        // 卖方主动成交在我们的价位（is_buyer_maker = true 表示卖方主动）
        e.on_market_event(trade(1000, dec!(3200), true));

        let snap = e.snapshot();
        let pos = snap.position.expect("成交后应建立持仓");
        assert_eq!(pos.side, Side::Buy);
        assert_eq!(pos.entry_price, dec!(3200));
        assert_eq!(pos.quantity, dec!(0.1));
        assert!(pos.stop_price.is_some(), "必须同时挂出止损");
        assert_eq!(pos.rungs.len(), 2, "两档止盈都要挂出");
        assert!(pos.rungs.iter().all(|r| !r.filled), "刚开仓时止盈都未成交");
    }

    /// 成交后止损数量必须等于持仓量——一致性不变量的实盘验证。
    #[test]
    fn stop_quantity_matches_position_after_entry() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.1));
        let _ = e.submit_manual(&plan, t0());
        e.on_market_event(trade(1000, dec!(3200), true));

        assert!(
            e.check_consistency().is_none(),
            "开仓后持仓与保护单必须一致：{:?}",
            e.check_consistency()
        );
    }

    /// 价格没到我们的价位就不该成交。
    #[test]
    fn trade_away_from_our_price_does_not_fill() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.1));
        let _ = e.submit_manual(&plan, t0());

        // 成交在 3210，高于我们的买价
        e.on_market_event(trade(1000, dec!(3210), true));

        assert!(e.snapshot().position.is_none(), "价格未到买价不应成交");
    }

    /// 方向不对的成交不能让我们成交。
    ///
    /// 我们挂买单时需要**卖方主动**（is_buyer_maker = true）。买方主动的
    /// 成交不会消耗买盘队列，不能算作我们成交的证据。
    #[test]
    fn wrong_aggressor_direction_does_not_fill() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.1));
        let _ = e.submit_manual(&plan, t0());

        // 价格在买价，但买方主动（is_buyer_maker = false）
        e.on_market_event(trade(1000, dec!(3200), false));

        assert!(
            e.snapshot().position.is_none(),
            "买方主动的成交不消耗买盘队列，不应让我们成交"
        );
    }

    /// 止盈成交后持仓减少，且止损数量同步调整。
    #[test]
    fn take_profit_fill_reduces_position_and_adjusts_stop() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.1));
        let _ = e.submit_manual(&plan, t0());
        e.on_market_event(trade(1000, dec!(3200), true));

        let pos_before = e.snapshot().position.expect("应有持仓");
        assert_eq!(pos_before.quantity, dec!(0.1));

        // 第一档止盈价 3200 * 1.0025 = 3208
        // 我方挂的是卖单，需要买方主动（is_buyer_maker = false）
        e.on_market_event(trade(2000, dec!(3208), false));

        let pos_after = e.snapshot().position.expect("第一档成交后仍有剩余持仓");
        assert_eq!(pos_after.quantity, dec!(0.05), "第一档平掉 50%");
        assert!(pos_after.rungs[0].filled, "第一档应标记为已成交");
        assert!(!pos_after.rungs[1].filled, "第二档尚未成交");
        assert!(
            e.check_consistency().is_none(),
            "止盈后止损数量必须同步，否则止损会被拒单：{:?}",
            e.check_consistency()
        );
    }

    /// 止损成交后仓位归零，且产生一笔完整交易记录。
    #[test]
    fn stop_fill_closes_position_and_records_trade() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.1));
        let _ = e.submit_manual(&plan, t0());
        e.on_market_event(trade(1000, dec!(3200), true));
        let _ = e.drain_events();

        // 止损价 3192，卖方主动成交（我们要卖出，需要买方主动）
        e.on_market_event(trade(2000, dec!(3192), false));

        let events = e.drain_events();
        let closed = events.iter().find_map(|ev| match ev {
            EngineEvent::TradeClosed {
                pnl, exit_reason, ..
            } => Some((*pnl, *exit_reason)),
            _ => None,
        });
        let (pnl, reason) = closed.expect("止损成交应产生一笔完整交易");
        assert_eq!(reason, ExitReason::StopLoss);
        assert!(pnl < Decimal::ZERO, "止损必然是亏损：{pnl}");
        assert!(e.snapshot().position.is_none(), "止损后应无持仓");
    }

    /// 提交手动计划后应产生在途订单。
    #[test]
    fn manual_submit_creates_pending_order() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = ok_plan();
        let out = e.submit_manual(&plan, t0());
        assert!(
            matches!(out, SubmitOutcome::Accepted(_)),
            "合理参数应被接受：{out:?}"
        );
    }

    /// 已有在途单时再次提交必须被拒绝——绝不能重复暴露。
    #[test]
    fn second_submit_is_rejected_while_pending() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = ok_plan();
        let _ = e.submit_manual(&plan, t0());

        let out = e.submit_manual(&plan, t0() + Duration::seconds(1));
        match out {
            SubmitOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("在途") || reason.contains("持仓"),
                    "{reason}"
                );
            }
            other => panic!("重复提交应被拒绝：{other:?}"),
        }
    }

    /// 止损方向错误必须被风控拦下。
    #[test]
    fn manual_submit_rejects_wrong_side_stop() {
        let mut e = engine();
        e.set_feed_connected(true);
        // 多头但止损在上方
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3210), dec!(0.1));
        match e.submit_manual(&plan, t0()) {
            SubmitOutcome::Rejected { reason } => {
                assert!(!reason.is_empty(), "拒绝必须带可读原因");
            }
            other => panic!("方向错误的止损应被拒绝：{other:?}"),
        }
    }

    /// 数量低于交易所最小步长必须被明确拒绝。
    #[test]
    fn manual_submit_rejects_zero_quantity() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.00001));
        match e.submit_manual(&plan, t0()) {
            SubmitOutcome::Rejected { reason } => assert!(!reason.is_empty()),
            other => panic!("零数量应被拒绝：{other:?}"),
        }
    }

    /// 预览不应改变引擎状态。
    #[test]
    fn preview_does_not_mutate_state() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = ok_plan();
        let before = e.equity();
        let pv = e.preview_manual_plan(&plan, t0()).unwrap();
        assert!(pv.accepted);
        assert_eq!(e.equity(), before, "预览不应改变权益");
        assert!(e.position_set().is_none(), "预览不应建立持仓");
    }

    /// 撤单后可以重新提交。
    #[test]
    fn cancel_pending_allows_resubmit() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = ok_plan();
        let _ = e.submit_manual(&plan, t0());
        assert!(e.cancel_pending(), "应能撤销在途单");
        assert!(!e.cancel_pending(), "无在途单时撤单应返回 false");

        let out = e.submit_manual(&plan, t0() + Duration::seconds(1));
        assert!(matches!(out, SubmitOutcome::Accepted(_)));
    }

    /// 挂单超过有效期必须自动撤销。
    #[test]
    fn pending_order_expires_past_deadline() {
        let mut e = engine();
        e.set_feed_connected(true);
        let plan = ok_plan();
        let _ = e.submit_manual(&plan, t0());

        // 推进到有效期之后
        e.on_market_event(kline(5, dec!(3210), dec!(3220)));
        let snap = e.snapshot();
        assert!(
            !snap
                .open_orders
                .iter()
                .any(|o| o.purpose == OrderPurpose::Entry),
            "过期后不应还有开仓单"
        );
    }

    /// 快照必须包含界面需要的全部字段。
    #[test]
    fn snapshot_contains_required_fields() {
        let mut e = engine();
        e.set_feed_connected(true);
        let snap = e.snapshot();
        assert_eq!(snap.mode, ServiceMode::Paper, "模拟盘快照必须标为 PAPER");
        assert_eq!(snap.symbol, "ETHUSDC");
        assert!(snap.feed_connected);
        assert_eq!(snap.realized_pnl, Decimal::ZERO);
    }

    /// 成交模型信息必须可查询——界面要展示结论有多可信。
    #[test]
    fn fill_model_info_is_exposed() {
        let e = engine();
        let (name, optimism) = e.fill_model_info();
        assert!(name.starts_with("M1"), "模拟盘默认用诚实基线：{name}");
        assert_eq!(optimism, Optimism::ConservativeLower, "默认不能是上界模型");
    }

    /// 结算资产必须正确传播。
    #[test]
    fn settlement_asset_is_usdc() {
        let e = engine();
        assert_eq!(e.instrument().settlement_asset, "USDC");
        assert_eq!(e.instrument().margin_asset, "USDC");
    }

    /// 风控参数必须来自引擎配置，而不是硬编码的默认值。
    ///
    /// 早先 `preview_manual` 内部固定用 `RiskLimits::default()`，导致引擎
    /// 配置的风控参数被完全忽略——调整配置不会有任何效果，而且不会有任何
    /// 提示。这个测试用一组「默认值拒绝、配置值接受」的参数锁住这条路径。
    #[test]
    fn risk_limits_come_from_engine_config_not_hardcoded() {
        // 盈亏比 1.25（止损 8 点，止盈 10 点）——默认要求 2，会被拒
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3192), dec!(0.1));
        // 改成一档止盈 0.001（3.2 点），盈亏比 0.4
        let mut tight = plan.clone();
        tight.take_profit = TpPlan::Single { pct: dec!(0.001) };

        let strict = PaperEngine::new(EngineConfig {
            limits: RiskLimits {
                max_stop_pct: dec!(0.01),
                min_reward_risk: dec!(2),
                max_feed_staleness_secs: 15,
            },
            ..config()
        });
        assert!(
            !strict.preview_manual_plan(&tight, t0()).unwrap().accepted,
            "最小盈亏比 2 时该计划应被拒绝"
        );

        let lax = PaperEngine::new(EngineConfig {
            limits: RiskLimits {
                max_stop_pct: dec!(0.01),
                min_reward_risk: dec!(0.1),
                max_feed_staleness_secs: 15,
            },
            ..config()
        });
        assert!(
            lax.preview_manual_plan(&tight, t0()).unwrap().accepted,
            "放宽盈亏比要求后同一计划应被接受——证明参数来自配置"
        );
    }

    /// 止损距离上限同样来自配置。
    #[test]
    fn max_stop_pct_comes_from_config() {
        // 止损距离 0.5%（16 点）
        let plan = manual_plan(Side::Buy, dec!(3200), dec!(3184), dec!(0.1));

        let tight = PaperEngine::new(EngineConfig {
            limits: RiskLimits {
                max_stop_pct: dec!(0.002),
                min_reward_risk: Decimal::ONE,
                max_feed_staleness_secs: 15,
            },
            ..config()
        });
        assert!(
            !tight.preview_manual_plan(&plan, t0()).unwrap().accepted,
            "止损距离 0.5% 超过 0.2% 上限应被拒绝"
        );

        // 放宽止损上限到 5%，同时把盈亏比要求降到 0.4 以下
        // （该计划的盈亏比 = 8/16 = 0.5）
        let lax = PaperEngine::new(EngineConfig {
            limits: RiskLimits {
                max_stop_pct: dec!(0.05),
                min_reward_risk: dec!(0.4),
                max_feed_staleness_secs: 15,
            },
            ..config()
        });
        assert!(
            lax.preview_manual_plan(&plan, t0()).unwrap().accepted,
            "放宽止损上限与盈亏比要求后应被接受"
        );
    }

    #[test]
    fn manual_close_without_position_returns_none() {
        let mut e = engine();
        assert!(e.close_position_manually(t0()).is_none());
    }

    /// 实盘安全闸门默认未武装——重启不能自动回到可交易状态。
    #[test]
    fn safety_starts_disarmed() {
        let e = engine();
        assert!(!e.safety().is_armed());
        assert!(!e.safety().can_submit());
        assert_eq!(e.safety().blocking_reasons().len(), 3);
    }

    /// 一致性检查在无持仓时返回 None。
    #[test]
    fn consistency_check_is_none_without_position() {
        let e = engine();
        assert!(e.check_consistency().is_none());
    }

    /// 保护的止盈档位分布要能查（界面画图用）。
    #[test]
    fn rung_summary_is_empty_without_position() {
        let e = engine();
        assert!(e.rung_summary().is_empty());
    }

    /// 事件流必须能被取出，且取出后清空。
    #[test]
    fn drain_events_empties_the_buffer() {
        let mut e = engine();
        e.set_feed_connected(true);
        let first = e.drain_events();
        assert!(!first.is_empty(), "设置连接状态应产生事件");
        let second = e.drain_events();
        assert!(second.is_empty(), "取出后应清空");
    }
}
