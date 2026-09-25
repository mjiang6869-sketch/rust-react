//! 保护单规划器：止盈、止损、保本止损、移动止损、分批止盈、定时取消。
//!
//! # 这是本仓库最重要的一个模块
//!
//! 旧实现把止盈价计算写在了三个地方，并且**已经分叉**：
//! `backtest.rs` 对多空两侧都用向下取整，`paper.rs` 和 `live.rs` 对买入用
//! 向上取整。回测因此在一个实盘永远不会挂出的价格上成交多单止盈。
//!
//! 这里把所有出场价格数学收敛到两个纯函数：`compile`（成交后建保护单）和
//! `on_market`（行情推进时调整）。因为输入只有 `&Instrument` / `&Position` /
//! `&[MarketEvent]`，它们天然被三种模式共用，也天然可表驱动测试。
//!
//! **舍入方向不由本模块手选**，而是通过 `Precision::price_for` + `PriceRole`
//! 声明用途，由精度模块唯一决定。

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::instrument::Instrument;
use crate::money::{Price, Qty};
use crate::order::{Alarm, ClientOrderId, Effect, Order, OrderPurpose, Side, TimeInForce};
use crate::precision::PriceRole;
use crate::state::{Position, TrackedOrder};

/// 止损规格。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StopSpec {
    /// 结构性止损：从信号推导出的具体价位（例如区间上沿外一跳）。
    Structural { price: Decimal },
    /// 固定百分比止损：距入场价 `pct`。
    FixedPct { pct: Decimal },
}

impl StopSpec {
    pub fn resolve(&self, entry: Decimal, side: Side) -> Decimal {
        match self {
            StopSpec::Structural { price } => *price,
            StopSpec::FixedPct { pct } => {
                let offset = entry * pct;
                match side {
                    Side::Buy => entry - offset,
                    Side::Sell => entry + offset,
                }
            }
        }
    }
}

/// 单档止盈。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TpRung {
    /// 距入场价的百分比（例如 0.0004 = 4bp）。
    #[serde(with = "rust_decimal::serde::str")]
    pub pct: Decimal,
    /// 该档平掉的仓位比例（0.4 = 平掉 40%）。
    #[serde(with = "rust_decimal::serde::str")]
    pub fraction: Decimal,
}

/// 止盈计划。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TpPlan {
    /// 单档全平。
    Single { pct: Decimal },
    /// 分批止盈。各档 `fraction` 之和必须 ≤ 1。
    Ladder { rungs: Vec<TpRung> },
}

impl TpPlan {
    /// 校验各档比例之和不超过 1。
    pub fn validate(&self) -> Result<(), DomainError> {
        match self {
            TpPlan::Single { .. } => Ok(()),
            TpPlan::Ladder { rungs } => {
                if rungs.is_empty() {
                    return Err(DomainError::IllegalTransition("分批止盈不能为空".into()));
                }
                let total: Decimal = rungs.iter().map(|r| r.fraction).sum();
                if total > Decimal::ONE {
                    return Err(DomainError::IllegalTransition(format!(
                        "分批止盈各档比例之和 {total} 超过 1"
                    )));
                }
                for (i, r) in rungs.iter().enumerate() {
                    if r.fraction <= Decimal::ZERO || r.pct <= Decimal::ZERO {
                        return Err(DomainError::IllegalTransition(format!(
                            "第 {} 档的比例和百分比必须为正数",
                            i + 1
                        )));
                    }
                }
                Ok(())
            }
        }
    }

    /// 展开为 (止盈百分比, 平仓比例) 列表。
    pub fn rungs(&self) -> Vec<(Decimal, Decimal)> {
        match self {
            TpPlan::Single { pct } => vec![(*pct, Decimal::ONE)],
            TpPlan::Ladder { rungs } => rungs.iter().map(|r| (r.pct, r.fraction)).collect(),
        }
    }
}

/// 保本止损：价格走到 `trigger_r` 倍止损距离后，把止损推到入场价 ± `offset`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakEvenSpec {
    /// 触发倍数。1 = 走出与止损距离相等的浮盈后触发。
    #[serde(with = "rust_decimal::serde::str")]
    pub trigger_r: Decimal,
    /// 保本后止损相对入场价的偏移（正数表示让出一点空间）。
    #[serde(with = "rust_decimal::serde::str")]
    pub offset: Decimal,
}

/// 移动止损。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailingSpec {
    /// 距最优价的距离。
    #[serde(with = "rust_decimal::serde::str")]
    pub distance: Decimal,
    /// 浮盈达到该价位后才开始移动（`None` = 立即）。
    pub activate_at: Option<Decimal>,
}

/// 完整保护计划。策略和手动面板共用同一个类型。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionPlan {
    pub stop: StopSpec,
    pub break_even: Option<BreakEvenSpec>,
    pub trailing: Option<TrailingSpec>,
    /// 挂单超时自动撤销的时刻。映射为交易所侧 GTD。
    pub timed_cancel: Option<DateTime<Utc>>,
}

/// 规划器产出的动作。全部是声明，编排层负责执行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtectionAction {
    Place(Box<Order>),
    Cancel(ClientOrderId),
    /// 改价（保本止损或移动止损推进）。
    Replace {
        id: ClientOrderId,
        new_price: Price,
    },
    /// 告警：止损未能成交，仓位裸露中。
    Alarm(Alarm),
}

/// 市场切片：规划器允许看的数据。刻意只给价格极值，不给完整 K 线，
/// 防止保护单逻辑意外依赖策略的指标计算。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarketSlice {
    /// 当前最优买价。
    pub bid: Price,
    /// 当前最优卖价。
    pub ask: Price,
    /// 自开仓以来的最高成交价（用于移动止损）。
    pub high_since_entry: Price,
    /// 自开仓以来的最低成交价。
    pub low_since_entry: Price,
}

impl MarketSlice {
    pub fn mid(&self) -> Price {
        Price::new((self.bid.get() + self.ask.get()) / Decimal::TWO)
    }
}

/// 一笔入场成交。`compile` 的输入，聚成类型而非散参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryFill {
    pub symbol: String,
    /// 产生这笔成交的入场单 ID。保护单 ID 从它派生。
    pub entry_order_id: ClientOrderId,
    /// 持仓方向（即入场单方向）。
    pub side: Side,
    /// **实际成交均价**，不是意图入场价——滑点必须反映到止损止盈基准里。
    pub price: Price,
    /// 本次成交数量。
    pub quantity: Qty,
}

/// 保护单规划器。无状态，所有方法都是纯函数。
pub struct ProtectionPlanner;

impl ProtectionPlanner {
    /// 入场成交后，生成保护单。
    ///
    /// 返回的订单全部是 reduce-only 的 post-only 单。止盈可按 `tp` 展开为
    /// 多档；某档数量低于交易所 step 时**跳过该档**而非整体失败。
    pub fn compile(
        instrument: &Instrument,
        fill: &EntryFill,
        plan: &ProtectionPlan,
        tp: &TpPlan,
    ) -> Result<Vec<ProtectionAction>, DomainError> {
        tp.validate()?;

        let entry = fill.price.get();
        let close_side = fill.side.opposite();
        let mut actions = Vec::new();

        // --- 止损 ---
        let stop_raw = plan.stop.resolve(entry, fill.side);
        let stop_price =
            instrument
                .precision
                .price_for(close_side, stop_raw, PriceRole::StopLoss)?;
        actions.push(ProtectionAction::Place(Box::new(Order {
            client_id: fill.entry_order_id.child("stop"),
            symbol: fill.symbol.clone(),
            purpose: OrderPurpose::StopLoss,
            side: close_side,
            quantity: fill.quantity,
            limit_price: stop_price,
            tif: TimeInForce::PostOnly,
            parent: Some(fill.entry_order_id.clone()),
        })));

        // --- 止盈（可能多档）---
        let rungs = tp.rungs();
        for (i, (pct, fraction)) in rungs.iter().enumerate() {
            let tp_raw = match fill.side {
                Side::Buy => entry * (Decimal::ONE + pct),
                Side::Sell => entry * (Decimal::ONE - pct),
            };
            let tp_price =
                instrument
                    .precision
                    .price_for(close_side, tp_raw, PriceRole::TakeProfit)?;
            // 用 `quantity_or_zero` 而非 `quantity`：某一档算出来低于交易所
            // step 时应当**跳过该档**，而不是让整个保护单计划失败。跳过是安全的
            // ——剩余仓位仍由止损和其余档位覆盖。
            let qty = instrument
                .precision
                .quantity_or_zero(fill.quantity.get() * fraction)?;
            if qty.is_zero() {
                continue;
            }
            let tag = if rungs.len() == 1 {
                "tp".to_string()
            } else {
                format!("tp{i}")
            };
            actions.push(ProtectionAction::Place(Box::new(Order {
                client_id: fill.entry_order_id.child(&tag),
                symbol: fill.symbol.clone(),
                purpose: OrderPurpose::TakeProfit,
                side: close_side,
                quantity: qty,
                limit_price: tp_price,
                tif: TimeInForce::PostOnly,
                parent: Some(fill.entry_order_id.clone()),
            })));
        }

        Ok(actions)
    }

    /// 行情推进时重新评估保护单。返回需要执行的调整。
    ///
    /// 处理三件事，顺序不能变：
    /// 1. 保本止损推进（只会让止损更靠近入场价，不会反向放松）
    /// 2. 移动止损推进（只会让止损更贴近价格，不会反向放松）
    /// 3. 定时取消到期的挂单
    ///
    /// 以及一个告警：止损单长时间未成交说明仓位在裸露。
    pub fn on_market(
        instrument: &Instrument,
        symbol: &str,
        position: &Position,
        working: &[&TrackedOrder],
        market: &MarketSlice,
        plan: &ProtectionPlan,
        now: DateTime<Utc>,
    ) -> Result<Vec<ProtectionAction>, DomainError> {
        let mut actions = Vec::new();
        let close_side = position.side.opposite();
        let entry = position.entry_price.get();
        let current = market.mid().get();

        let stop_order = working
            .iter()
            .find(|t| t.order.purpose == OrderPurpose::StopLoss && t.state.is_open());
        let current_stop = position
            .stop_price
            .map(|p| p.get())
            .or_else(|| stop_order.map(|t| t.order.limit_price.get()));

        // --- 1 & 2：计算期望的止损价 ---
        let mut desired_stop: Option<Decimal> = None;

        // 保本止损：浮盈达到 trigger_r 倍止损距离后，把止损推到入场价附近
        //
        // 风险距离从**计划**推导（`plan.stop.resolve`），而不是从当前挂着的
        // 止损单推导。两者在正常路径下相同，但前者在止损单尚未挂出、或已因
        // post-only 被静默拒绝而消失时依然成立——那些正是最需要保本保护的
        // 时刻（仓位在裸露）。
        if let Some(be) = plan.break_even {
            let risk_distance = (entry - plan.stop.resolve(entry, position.side)).abs();
            let favorable = match position.side {
                Side::Buy => current - entry,
                Side::Sell => entry - current,
            };
            if risk_distance > Decimal::ZERO && favorable >= risk_distance * be.trigger_r {
                let breakeven = match position.side {
                    Side::Buy => entry + be.offset,
                    Side::Sell => entry - be.offset,
                };
                desired_stop = Some(breakeven);
            }
        }

        // 移动止损：取更贴近当前价的那个（对多头是更高，对空头是更低）
        if let Some(tr) = plan.trailing {
            let activated = match tr.activate_at {
                None => true,
                Some(level) => match position.side {
                    Side::Buy => market.high_since_entry.get() >= level,
                    Side::Sell => market.low_since_entry.get() <= level,
                },
            };
            if activated {
                let trailed = match position.side {
                    Side::Buy => market.high_since_entry.get() - tr.distance,
                    Side::Sell => market.low_since_entry.get() + tr.distance,
                };
                desired_stop = Some(match desired_stop {
                    Some(existing) => match position.side {
                        // 多头止损只能上移
                        Side::Buy => existing.max(trailed),
                        // 空头止损只能下移
                        Side::Sell => existing.min(trailed),
                    },
                    None => trailed,
                });
            }
        }

        // 应用到 STOP 方向上"只收紧，不放松"的不变量。
        //
        // 注意 `current_stop` 为 `None` 的情形：那是**止损尚未挂出或已消失**
        // （post-only 被静默拒绝后币安不保留记录），此时仓位完全裸露，任何
        // 有效止损都是改善，必须挂出去。早先的写法把这一情形和"不需要调整"
        // 混为一谈，会让最危险的时刻反而没有止损。
        if let Some(desired) = desired_stop {
            let strictly_better = match current_stop {
                // 已有止损：只能朝有利方向推进
                Some(existing) => match position.side {
                    Side::Buy => desired > existing,
                    Side::Sell => desired < existing,
                },
                // 没有止损：任何有效止损都值得挂
                None => true,
            };

            if strictly_better {
                let quantized =
                    instrument
                        .precision
                        .price_for(close_side, desired, PriceRole::StopLoss)?;
                match stop_order {
                    Some(t) => actions.push(ProtectionAction::Replace {
                        id: t.order.client_id.clone(),
                        new_price: quantized,
                    }),
                    None => {
                        // ID 必须**确定性**：不能包含当前时间戳或随机量，
                        // 否则 `on_market` 每次被调用都会生成一个新 ID，导致
                        // 重复挂单。复用持仓的开仓时刻作为序号，保证同一仓位
                        // 在任何时候重新计算都得到同一个 ID。
                        let seq = position.opened_at.timestamp_millis().max(0) as u64;
                        let base = ClientOrderId::new(
                            &format!("{symbol}:restop:{}", position.side_stable_tag()),
                            seq,
                        );
                        actions.push(ProtectionAction::Place(Box::new(Order {
                            client_id: base,
                            symbol: symbol.to_string(),
                            purpose: OrderPurpose::StopLoss,
                            side: close_side,
                            quantity: position.quantity,
                            limit_price: quantized,
                            tif: TimeInForce::PostOnly,
                            parent: None,
                        })));
                    }
                }
            }
        }

        // --- 3：定时取消 ---
        if let Some(deadline) = plan.timed_cancel {
            if now >= deadline {
                for t in working {
                    if t.order.purpose.is_entry() && t.state.is_open() {
                        actions.push(ProtectionAction::Cancel(t.order.client_id.clone()));
                    }
                }
            }
        }

        // --- 告警：止损挂着但未成交 ---
        // maker-only 特有风险：止损也是挂单，跳空穿过它且不回来时，
        // 没有任何机制会平掉仓位。必须让操作者看到。
        if let Some(t) = stop_order {
            let triggered = match position.side {
                Side::Buy => market.bid.get() <= t.order.limit_price.get(),
                Side::Sell => market.ask.get() >= t.order.limit_price.get(),
            };
            if triggered {
                let since = t.updated_at;
                let exposed_for = now - since;
                if exposed_for > chrono::Duration::seconds(30) {
                    actions.push(ProtectionAction::Alarm(Alarm::StopUnfilledExposure {
                        order: t.order.client_id.clone(),
                        since,
                    }));
                }
            }
        }

        Ok(actions)
    }
}

/// 把保护单动作转换成状态机副作用。编排层用它把规划器接进订单生命周期。
pub fn actions_to_effects(actions: Vec<ProtectionAction>) -> Vec<Effect> {
    actions
        .into_iter()
        .map(|a| match a {
            ProtectionAction::Place(o) => Effect::Submit(o),
            ProtectionAction::Cancel(id) => Effect::Cancel(id),
            // 改价在交易所侧是"撤旧挂新"。为保持 maker-only 语义，
            // 必须先撤成功再挂，避免两张单同时在挂造成超卖。
            ProtectionAction::Replace { id, .. } => Effect::Cancel(id),
            ProtectionAction::Alarm(a) => Effect::Alarm(a),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::{ContractKind, FeeSchedule, FeeSource};
    use crate::precision::Precision;
    use rust_decimal_macros::dec;

    fn instr() -> Instrument {
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

    fn position(side: Side, entry: Decimal, qty: Decimal) -> Position {
        Position {
            symbol: "ETHUSDC".into(),
            side,
            quantity: Qty::new(qty),
            entry_price: Price::new(entry),
            opened_at: Utc::now(),
            stop_price: None,
        }
    }

    fn plan(stop: StopSpec) -> ProtectionPlan {
        ProtectionPlan {
            stop,
            break_even: None,
            trailing: None,
            timed_cancel: None,
        }
    }

    fn entry_id() -> ClientOrderId {
        ClientOrderId::new("mm", 1)
    }

    fn entry(side: Side, price: Decimal, qty: Decimal) -> EntryFill {
        EntryFill {
            symbol: "ETHUSDC".into(),
            entry_order_id: entry_id(),
            side,
            price: Price::new(price),
            quantity: Qty::new(qty),
        }
    }

    // ---------- compile ----------

    /// 这一条就是旧实现的分叉点：买入止盈必须是向上取整的价格。
    #[test]
    fn buy_take_profit_is_quantized_up_matching_live_behavior() {
        let i = instr();
        let actions = ProtectionPlanner::compile(
            &i,
            &entry(Side::Buy, dec!(3200), dec!(1)),
            &plan(StopSpec::Structural { price: dec!(3190) }),
            &TpPlan::Single { pct: dec!(0.00004) },
        )
        .unwrap();

        let tp = actions
            .iter()
            .find_map(|a| match a {
                ProtectionAction::Place(o) if o.purpose == OrderPurpose::TakeProfit => Some(o),
                _ => None,
            })
            .expect("必须有止盈单");
        // 3200 * 1.00004 = 3200.128 -> 向上取整 3200.13
        assert_eq!(tp.limit_price.get(), dec!(3200.13));
        assert_eq!(tp.side, Side::Sell, "多头止盈是卖出");
        assert!(tp.reduce_only());
    }

    #[test]
    fn every_generated_order_is_reduce_only() {
        let i = instr();
        let actions = ProtectionPlanner::compile(
            &i,
            &entry(Side::Buy, dec!(3200), dec!(1)),
            &plan(StopSpec::FixedPct { pct: dec!(0.003) }),
            &TpPlan::Single { pct: dec!(0.0004) },
        )
        .unwrap();

        for a in &actions {
            if let ProtectionAction::Place(o) = a {
                assert!(o.reduce_only(), "保护单 {} 必须 reduce-only", o.client_id);
                assert_eq!(o.side, Side::Sell, "多头仓位的出场方向应为卖出");
                assert_eq!(o.parent.as_ref(), Some(&entry_id()));
            }
        }
    }

    #[test]
    fn ladder_take_profit_splits_quantity_by_fraction() {
        let i = instr();
        let actions = ProtectionPlanner::compile(
            &i,
            &entry(Side::Buy, dec!(3200), dec!(1)),
            &plan(StopSpec::Structural { price: dec!(3190) }),
            &TpPlan::Ladder {
                rungs: vec![
                    TpRung {
                        pct: dec!(0.0004),
                        fraction: dec!(0.4),
                    },
                    TpRung {
                        pct: dec!(0.0008),
                        fraction: dec!(0.3),
                    },
                    TpRung {
                        pct: dec!(0.0012),
                        fraction: dec!(0.3),
                    },
                ],
            },
        )
        .unwrap();

        let tps: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                ProtectionAction::Place(o) if o.purpose == OrderPurpose::TakeProfit => Some(o),
                _ => None,
            })
            .collect();

        assert_eq!(tps.len(), 3, "三档止盈应生成三张单");
        assert_eq!(tps[0].quantity.get(), dec!(0.4));
        assert_eq!(tps[1].quantity.get(), dec!(0.3));
        assert_eq!(tps[2].quantity.get(), dec!(0.3));

        // 档位越高，价格越有利（多头是越来越高的卖价）
        assert!(tps[0].limit_price.get() < tps[1].limit_price.get());
        assert!(tps[1].limit_price.get() < tps[2].limit_price.get());

        // 三档合计等于持仓量，不能超卖
        let total: Decimal = tps.iter().map(|o| o.quantity.get()).sum();
        assert_eq!(total, dec!(1));

        // 单档时用 "tp"，多档时用 "tp0/tp1/tp2"，ID 必须唯一
        let ids: std::collections::HashSet<_> = tps.iter().map(|o| o.client_id.clone()).collect();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn ladder_fractions_over_one_are_rejected() {
        let tp = TpPlan::Ladder {
            rungs: vec![
                TpRung {
                    pct: dec!(0.001),
                    fraction: dec!(0.7),
                },
                TpRung {
                    pct: dec!(0.002),
                    fraction: dec!(0.7),
                },
            ],
        };
        assert!(tp.validate().is_err(), "合计 1.4 必须被拒绝");

        let i = instr();
        let err = ProtectionPlanner::compile(
            &i,
            &entry(Side::Buy, dec!(3200), dec!(1)),
            &plan(StopSpec::Structural { price: dec!(3190) }),
            &tp,
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::IllegalTransition(_)));
    }

    /// 窄到低于最小数量的档位应被跳过而不是生成非法订单。
    #[test]
    fn rung_below_min_quantity_is_skipped() {
        let i = instr();
        let actions = ProtectionPlanner::compile(
            &i,
            &entry(Side::Buy, dec!(3200), dec!(0.002)),
            &plan(StopSpec::Structural { price: dec!(3190) }),
            &TpPlan::Ladder {
                rungs: vec![
                    TpRung {
                        pct: dec!(0.0004),
                        fraction: dec!(0.05),
                    }, // 0.0001 < min_qty
                    TpRung {
                        pct: dec!(0.0008),
                        fraction: dec!(0.95),
                    },
                ],
            },
        )
        .unwrap();

        let tps: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                ProtectionAction::Place(o) if o.purpose == OrderPurpose::TakeProfit => Some(o),
                _ => None,
            })
            .collect();
        assert_eq!(tps.len(), 1, "过小档位应被跳过");
    }

    #[test]
    fn short_position_exit_orders_are_buys() {
        let i = instr();
        let actions = ProtectionPlanner::compile(
            &i,
            &entry(Side::Sell, dec!(3200), dec!(1)),
            &plan(StopSpec::Structural { price: dec!(3210) }),
            &TpPlan::Single { pct: dec!(0.0004) },
        )
        .unwrap();

        for a in &actions {
            if let ProtectionAction::Place(o) = a {
                assert_eq!(o.side, Side::Buy, "空头仓位的出场方向应为买入");
            }
        }
    }

    // ---------- on_market ----------

    #[test]
    fn break_even_stop_engages_after_trigger_r() {
        let i = instr();
        let pos = position(Side::Buy, dec!(3200), dec!(1));
        let mut p = plan(StopSpec::Structural { price: dec!(3190) });
        p.break_even = Some(BreakEvenSpec {
            trigger_r: Decimal::ONE,
            offset: Decimal::ZERO,
        });

        // 止损距离 10 点，触发需要浮盈 >= 10 点 -> 中间价 3210。
        // 尚未到：3205
        let early = MarketSlice {
            bid: Price::new(dec!(3204)),
            ask: Price::new(dec!(3206)),
            high_since_entry: Price::new(dec!(3206)),
            low_since_entry: Price::new(dec!(3199)),
        };
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[], &early, &p, Utc::now()).unwrap();
        assert!(actions.is_empty(), "未达到触发倍数不应调整止损");

        // 已到：3212
        let late = MarketSlice {
            bid: Price::new(dec!(3211)),
            ask: Price::new(dec!(3213)),
            high_since_entry: Price::new(dec!(3213)),
            low_since_entry: Price::new(dec!(3199)),
        };
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[], &late, &p, Utc::now()).unwrap();
        assert!(
            actions.iter().any(
                |a| matches!(a, ProtectionAction::Place(o) if o.purpose == OrderPurpose::StopLoss)
            ),
            "触发保本后应挂出止损单，实际：{actions:?}"
        );
    }

    /// 回归测试：止损单不存在时必须挂出止损，而不是跳过。
    ///
    /// 修复前的写法是 `if let (Some(desired), Some(existing))`，在
    /// `current_stop == None`（止损尚未挂出，或已被 post-only 静默拒绝后
    /// 消失）时整段跳过——仓位在**完全裸露**的状态下没有任何止损保护。
    /// 这是最危险的时刻反而没有保护。
    #[test]
    fn missing_stop_is_placed_not_skipped() {
        let i = instr();
        // 既没有 position.stop_price，也没有在途止损单
        let pos = position(Side::Buy, dec!(3200), dec!(1));
        assert!(pos.stop_price.is_none());

        let mut p = plan(StopSpec::Structural { price: dec!(3190) });
        p.break_even = Some(BreakEvenSpec {
            trigger_r: Decimal::ONE,
            offset: Decimal::ZERO,
        });

        // 浮盈 12 点 > 风险距离 10 点，保本应触发
        let market = MarketSlice {
            bid: Price::new(dec!(3211)),
            ask: Price::new(dec!(3213)),
            high_since_entry: Price::new(dec!(3213)),
            low_since_entry: Price::new(dec!(3199)),
        };
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[], &market, &p, Utc::now())
                .unwrap();

        let stop = actions
            .iter()
            .find_map(|a| match a {
                ProtectionAction::Place(o) if o.purpose == OrderPurpose::StopLoss => Some(o),
                _ => None,
            })
            .expect("没有现存止损时必须挂出止损，否则仓位裸露");
        // 保本 offset=0 -> 止损推到入场价 3200（卖单向下取整）
        assert_eq!(stop.limit_price.get(), dec!(3200));
        assert!(stop.reduce_only());
        assert_eq!(stop.quantity.get(), dec!(1), "止损应覆盖全部持仓");
    }

    /// 重新挂止损的订单 ID 必须是确定性的：同一持仓反复评估得到同一 ID，
    /// 否则每次 `on_market` 都会产生一张新订单，造成重复挂单。
    #[test]
    fn replacement_stop_id_is_deterministic_across_evaluations() {
        let i = instr();
        let pos = position(Side::Buy, dec!(3200), dec!(1));
        let mut p = plan(StopSpec::Structural { price: dec!(3190) });
        p.break_even = Some(BreakEvenSpec {
            trigger_r: Decimal::ONE,
            offset: Decimal::ZERO,
        });

        let market = MarketSlice {
            bid: Price::new(dec!(3211)),
            ask: Price::new(dec!(3213)),
            high_since_entry: Price::new(dec!(3213)),
            low_since_entry: Price::new(dec!(3199)),
        };

        let id_of = |now: DateTime<Utc>| {
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[], &market, &p, now)
                .unwrap()
                .into_iter()
                .find_map(|a| match a {
                    ProtectionAction::Place(o) if o.purpose == OrderPurpose::StopLoss => {
                        Some(o.client_id.clone())
                    }
                    _ => None,
                })
                .expect("应挂出止损")
        };

        // 两次评估使用不同的时刻，但订单 ID 必须相同
        let a = id_of(Utc::now());
        let b = id_of(Utc::now() + chrono::Duration::seconds(5));
        assert_eq!(a, b, "重复评估必须产生同一个止损订单 ID");
    }

    #[test]
    fn trailing_stop_only_moves_favorably() {
        let i = instr();
        let mut pos = position(Side::Buy, dec!(3200), dec!(1));
        pos.stop_price = Some(Price::new(dec!(3195)));
        let mut p = plan(StopSpec::Structural { price: dec!(3195) });
        p.trailing = Some(TrailingSpec {
            distance: dec!(10),
            activate_at: None,
        });

        // 价格最高走到 3230 -> 移动止损应到 3220，比现有 3195 更好
        let market = MarketSlice {
            bid: Price::new(dec!(3229)),
            ask: Price::new(dec!(3231)),
            high_since_entry: Price::new(dec!(3230)),
            low_since_entry: Price::new(dec!(3199)),
        };
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[], &market, &p, Utc::now())
                .unwrap();
        let replaced = actions
            .iter()
            .find_map(|a| match a {
                ProtectionAction::Place(o) if o.purpose == OrderPurpose::StopLoss => {
                    Some(o.limit_price.get())
                }
                _ => None,
            })
            .expect("应挂出新的移动止损");
        assert_eq!(replaced, dec!(3220));
    }

    /// 移动止损绝不能反向放松——价格回落后止损应保持不动。
    #[test]
    fn trailing_stop_never_loosens() {
        let i = instr();
        let mut pos = position(Side::Buy, dec!(3200), dec!(1));
        pos.stop_price = Some(Price::new(dec!(3220))); // 已被推到 3220
        let mut p = plan(StopSpec::Structural { price: dec!(3195) });
        p.trailing = Some(TrailingSpec {
            distance: dec!(10),
            activate_at: None,
        });

        // 价格回落到最高 3210 -> 计算出的跟踪止损 3200，低于现有 3220
        let market = MarketSlice {
            bid: Price::new(dec!(3205)),
            ask: Price::new(dec!(3207)),
            high_since_entry: Price::new(dec!(3210)),
            low_since_entry: Price::new(dec!(3199)),
        };
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[], &market, &p, Utc::now())
                .unwrap();
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ProtectionAction::Place(_))),
            "止损不应被放松，实际：{actions:?}"
        );
    }

    #[test]
    fn trailing_stop_respects_activation_level() {
        let i = instr();
        let pos = position(Side::Buy, dec!(3200), dec!(1));
        let mut p = plan(StopSpec::Structural { price: dec!(3190) });
        p.trailing = Some(TrailingSpec {
            distance: dec!(10),
            activate_at: Some(dec!(3250)),
        });

        // 最高只到 3230，未达激活价 3250
        let market = MarketSlice {
            bid: Price::new(dec!(3229)),
            ask: Price::new(dec!(3231)),
            high_since_entry: Price::new(dec!(3230)),
            low_since_entry: Price::new(dec!(3199)),
        };
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[], &market, &p, Utc::now())
                .unwrap();
        assert!(actions.is_empty(), "未达激活价不应启用移动止损");
    }

    #[test]
    fn timed_cancel_fires_at_deadline_not_before() {
        let i = instr();
        let pos = position(Side::Buy, dec!(3200), dec!(1));
        let now = Utc::now();
        let mut p = plan(StopSpec::Structural { price: dec!(3190) });
        p.timed_cancel = Some(now + chrono::Duration::seconds(120));

        let market = MarketSlice {
            bid: Price::new(dec!(3201)),
            ask: Price::new(dec!(3203)),
            high_since_entry: Price::new(dec!(3203)),
            low_since_entry: Price::new(dec!(3199)),
        };

        let entry = Order {
            client_id: ClientOrderId::new("mm", 9),
            symbol: "ETHUSDC".into(),
            purpose: OrderPurpose::Entry,
            side: Side::Buy,
            quantity: Qty::new(dec!(1)),
            limit_price: Price::new(dec!(3200)),
            tif: TimeInForce::PostOnly,
            parent: None,
        };
        let tracked = TrackedOrder {
            order: entry,
            state: crate::order::OrderState::Live,
            exchange_id: Some("E1".into()),
            filled: Qty::ZERO,
            avg_price: None,
            updated_at: now,
        };

        // 未到截止时间
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[&tracked], &market, &p, now)
                .unwrap();
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ProtectionAction::Cancel(_)))
        );

        // 到截止时间
        let later = now + chrono::Duration::seconds(121);
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[&tracked], &market, &p, later)
                .unwrap();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, ProtectionAction::Cancel(_)))
        );
    }

    /// maker-only 特有风险的可见性：止损挂着未成交必须告警。
    #[test]
    fn unfilled_stop_raises_exposure_alarm() {
        let i = instr();
        let pos = position(Side::Buy, dec!(3200), dec!(1));
        let p = plan(StopSpec::Structural { price: dec!(3190) });

        let stop_order = Order {
            client_id: ClientOrderId::new("mm", 1).child("stop"),
            symbol: "ETHUSDC".into(),
            purpose: OrderPurpose::StopLoss,
            side: Side::Sell,
            quantity: Qty::new(dec!(1)),
            limit_price: Price::new(dec!(3190)),
            tif: TimeInForce::PostOnly,
            parent: Some(ClientOrderId::new("mm", 1)),
        };
        let long_ago = Utc::now() - chrono::Duration::seconds(60);
        let tracked = TrackedOrder {
            order: stop_order,
            state: crate::order::OrderState::Live,
            exchange_id: Some("E1".into()),
            filled: Qty::ZERO,
            avg_price: None,
            updated_at: long_ago,
        };

        // 价格已跌破止损限价（bid <= 3190）但没有成交
        let market = MarketSlice {
            bid: Price::new(dec!(3185)),
            ask: Price::new(dec!(3187)),
            high_since_entry: Price::new(dec!(3210)),
            low_since_entry: Price::new(dec!(3185)),
        };
        let actions =
            ProtectionPlanner::on_market(&i, "ETHUSDC", &pos, &[&tracked], &market, &p, Utc::now())
                .unwrap();

        assert!(
            actions.iter().any(|a| matches!(
                a,
                ProtectionAction::Alarm(Alarm::StopUnfilledExposure { .. })
            )),
            "止损触发但未成交必须告警，实际：{actions:?}"
        );
    }

    #[test]
    fn replace_actions_become_cancel_then_place() {
        let effects = actions_to_effects(vec![
            ProtectionAction::Replace {
                id: ClientOrderId::new("mm", 1),
                new_price: Price::new(dec!(3220)),
            },
            ProtectionAction::Alarm(Alarm::ReconcileTimeout {
                order: ClientOrderId::new("mm", 2),
                since: Utc::now(),
            }),
        ]);

        assert!(matches!(effects[0], Effect::Cancel(_)), "改价必须先撤旧单");
        assert!(matches!(effects[1], Effect::Alarm(_)));
    }

    /// 固定百分比止损的解析方向必须正确。
    #[test]
    fn fixed_pct_stop_resolves_by_side() {
        let spec = StopSpec::FixedPct { pct: dec!(0.01) };
        assert_eq!(spec.resolve(dec!(3200), Side::Buy), dec!(3168)); // 3200 - 32
        assert_eq!(spec.resolve(dec!(3200), Side::Sell), dec!(3232)); // 3200 + 32
    }
}
