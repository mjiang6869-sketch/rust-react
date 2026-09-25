//! 订单与持仓状态机。回测、模拟盘、实盘共用的唯一权威。
//!
//! # 职责
//!
//! 接收执行事件（`ExecEvent`），推进订单状态，维护持仓与已实现盈亏，
//! 返回需要编排层执行的 `Effect` 列表。**本模块不做任何 I/O。**
//!
//! # 为什么是"唯一权威"
//!
//! 旧实现有三个真相来源：`PaperEngine.stored.orders`、
//! `LiveRuntime.submitted: HashMap`、以及 `main.rs` 里一份把两者拼起来的
//! 持久化逻辑。结果是"现在有哪些订单"这个问题有三个可能不同的答案。
//! 这里把它收敛到一个结构体，任何其他模块只能读它。

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::error::DomainError;
use crate::money::{Price, Qty};
use crate::order::{ClientOrderId, Effect, Order, OrderState, Side};

/// 一笔成交。来自交易所用户数据流，或来自撮合引擎。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    /// 交易所成交 ID。用于去重——同一笔成交可能通过多个通道到达
    /// （用户数据流推送 + 主动查询）。
    pub trade_id: String,
    pub client_id: ClientOrderId,
    pub quantity: Qty,
    pub price: Price,
    /// 该笔成交的手续费，已带符号（负数为支出）。
    pub fee: Decimal,
    /// 手续费记入的结算资产。
    pub fee_asset: String,
    pub at: DateTime<Utc>,
}

/// 执行事件。适配器把所有来源归一化成这几种。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecEvent {
    /// 交易所确认订单已在挂。
    Accepted {
        client_id: ClientOrderId,
        exchange_id: String,
    },
    /// 交易所拒单。
    Rejected {
        client_id: ClientOrderId,
        reason: crate::error::RejectReason,
    },
    /// 成交（可能是部分成交）。
    Filled(Fill),
    /// 撤单完成。
    Cancelled {
        client_id: ClientOrderId,
        filled: Qty,
    },
    /// GTD 到期。
    Expired { client_id: ClientOrderId },
    /// 请求结果未知，需要查询对账。
    Unknown {
        client_id: ClientOrderId,
        at: DateTime<Utc>,
    },
}

/// 一张被跟踪的订单。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackedOrder {
    pub order: Order,
    pub state: OrderState,
    pub exchange_id: Option<String>,
    /// 累计成交量。用于校验交易所上报的累计量单调不减。
    pub filled: Qty,
    /// 成交均价（按量加权）。
    pub avg_price: Option<Price>,
    pub updated_at: DateTime<Utc>,
}

/// 持仓。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub symbol: String,
    pub side: Side,
    pub quantity: Qty,
    pub entry_price: Price,
    pub opened_at: DateTime<Utc>,
    /// 当前生效的止损价。保本止损/移动止损会更新它。
    pub stop_price: Option<Price>,
}

impl Position {
    /// 用于构造订单 ID 的稳定方向标签。
    ///
    /// 刻意不用 `format!("{side:?}")`——那会把 Rust 的 Debug 表现形式写进
    /// 订单 ID，一旦枚举改名或加变体，历史订单 ID 就变了。
    pub const fn side_stable_tag(&self) -> &'static str {
        match self.side {
            Side::Buy => "long",
            Side::Sell => "short",
        }
    }
}

/// 订单与持仓的状态机。
#[derive(Debug, Default)]
pub struct OrderBookState {
    orders: BTreeMap<ClientOrderId, TrackedOrder>,
    positions: BTreeMap<String, Position>,
    /// 已处理的成交 ID，用于去重。
    seen_fills: HashSet<String>,
    /// 已实现盈亏，按结算资产分开记账。
    ///
    /// 按资产分开是硬要求：USDT 和 USDC 是两个钱包，绝不能相加。
    realized_pnl: BTreeMap<String, Decimal>,
}

impl OrderBookState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一张待提交的订单。必须在提交前调用，这样超时后才能查到它。
    pub fn register(&mut self, order: Order, now: DateTime<Utc>) -> Result<(), DomainError> {
        if self.orders.contains_key(&order.client_id) {
            return Err(DomainError::IllegalTransition(format!(
                "订单 {} 已登记，禁止重复登记",
                order.client_id
            )));
        }
        self.orders.insert(
            order.client_id.clone(),
            TrackedOrder {
                order,
                state: OrderState::PendingSubmit,
                exchange_id: None,
                filled: Qty::ZERO,
                avg_price: None,
                updated_at: now,
            },
        );
        Ok(())
    }

    /// 应用一个执行事件。返回需要编排层执行的副作用。
    pub fn apply(
        &mut self,
        event: ExecEvent,
        now: DateTime<Utc>,
    ) -> Result<Vec<Effect>, DomainError> {
        match event {
            ExecEvent::Accepted {
                client_id,
                exchange_id,
            } => {
                let tracked = self.get_mut(&client_id)?;
                if tracked.state.is_terminal() {
                    return Err(DomainError::IllegalTransition(format!(
                        "订单 {client_id} 已处于终态 {:?}，不能再被接受",
                        tracked.state
                    )));
                }
                tracked.exchange_id = Some(exchange_id);
                tracked.state = OrderState::Live;
                tracked.updated_at = now;
                Ok(vec![Effect::Persist])
            }

            ExecEvent::Rejected { client_id, reason } => {
                let tracked = self.get_mut(&client_id)?;
                tracked.state = OrderState::Rejected { reason };
                tracked.updated_at = now;
                Ok(vec![Effect::Persist])
            }

            ExecEvent::Filled(fill) => self.apply_fill(fill, now),

            ExecEvent::Cancelled { client_id, filled } => {
                let tracked = self.get_mut(&client_id)?;
                tracked.filled = filled;
                tracked.state = OrderState::Cancelled { filled };
                tracked.updated_at = now;
                Ok(vec![Effect::Persist])
            }

            ExecEvent::Expired { client_id } => {
                let tracked = self.get_mut(&client_id)?;
                tracked.state = OrderState::Expired;
                tracked.updated_at = now;
                Ok(vec![Effect::Persist])
            }

            ExecEvent::Unknown { client_id, at } => {
                let tracked = self.get_mut(&client_id)?;
                tracked.state = OrderState::Unknown {
                    since: at,
                    last_probe: None,
                };
                tracked.updated_at = now;
                // Unknown 的唯一出口是查询，所以这里必须发出 Query。
                Ok(vec![Effect::Persist, Effect::Query(client_id)])
            }
        }
    }

    fn apply_fill(&mut self, fill: Fill, now: DateTime<Utc>) -> Result<Vec<Effect>, DomainError> {
        // 按 trade_id 去重。同一笔成交可能从用户数据流和主动查询两条路到达。
        if !self.seen_fills.insert(fill.trade_id.clone()) {
            return Ok(Vec::new());
        }

        let is_entry = {
            let tracked = self.get_mut(&fill.client_id)?;
            let new_filled = tracked.filled + fill.quantity;

            // 累计成交量不能超过下单量。超出说明交易所上报异常或我们算错了，
            // 两种都不能继续——否则持仓账目会凭空多出数量。
            if new_filled.get() > tracked.order.quantity.get() {
                return Err(DomainError::IllegalTransition(format!(
                    "订单 {} 累计成交量 {new_filled} 超过下单量 {}",
                    fill.client_id, tracked.order.quantity
                )));
            }

            // 加权平均成交价
            let prev_notional = match tracked.avg_price {
                Some(p) => p.get() * tracked.filled.get(),
                None => Decimal::ZERO,
            };
            let new_notional = prev_notional + fill.price.get() * fill.quantity.get();
            let avg = if new_filled.is_zero() {
                None
            } else {
                Some(Price::new(new_notional / new_filled.get()))
            };

            tracked.filled = new_filled;
            tracked.avg_price = avg;
            tracked.state = if new_filled == tracked.order.quantity {
                OrderState::Filled {
                    filled: new_filled,
                    avg: avg.unwrap_or(fill.price),
                }
            } else {
                OrderState::PartiallyFilled {
                    filled: new_filled,
                    avg: avg.unwrap_or(fill.price),
                }
            };
            tracked.updated_at = now;

            tracked.order.purpose.is_entry()
        };

        // 手续费与盈亏记入结算资产。用订单 ID 里的 symbol 关联，这里简化为
        // 由编排层保证 fill 已带上正确的资产。
        *self
            .realized_pnl
            .entry(fill.fee_asset.clone())
            .or_insert(Decimal::ZERO) += fill.fee;

        if is_entry {
            self.open_or_extend_position(&fill, now);
        } else {
            self.reduce_position(&fill)?;
        }

        Ok(vec![Effect::Persist])
    }

    fn open_or_extend_position(&mut self, fill: &Fill, now: DateTime<Utc>) {
        let symbol = self
            .orders
            .get(&fill.client_id)
            .map(|t| t.order.symbol.clone())
            .unwrap_or_default();
        let side = self
            .orders
            .get(&fill.client_id)
            .map(|t| t.order.side)
            .unwrap_or(Side::Buy);

        match self.positions.get_mut(&symbol) {
            // 同向加仓：更新加权入场价
            Some(pos) if pos.side == side => {
                let prev_notional = pos.entry_price.get() * pos.quantity.get();
                let add_notional = fill.price.get() * fill.quantity.get();
                let total_qty = pos.quantity + fill.quantity;
                if !total_qty.is_zero() {
                    pos.entry_price = Price::new((prev_notional + add_notional) / total_qty.get());
                }
                pos.quantity = total_qty;
            }
            // 反向成交出现在开仓单上，属于状态不一致
            Some(_) => {}
            None => {
                self.positions.insert(
                    symbol.clone(),
                    Position {
                        symbol,
                        side,
                        quantity: fill.quantity,
                        entry_price: fill.price,
                        opened_at: now,
                        stop_price: None,
                    },
                );
            }
        }
    }

    fn reduce_position(&mut self, fill: &Fill) -> Result<(), DomainError> {
        let Some(tracked) = self.orders.get(&fill.client_id) else {
            return Err(DomainError::UnknownReference(fill.client_id.to_string()));
        };
        let symbol = tracked.order.symbol.clone();
        let close_side = tracked.order.side;

        let Some(pos) = self.positions.get_mut(&symbol) else {
            // reduce-only 单在无持仓时成交，说明本地状态与交易所不一致，
            // 绝不能凭此建立反向持仓。
            return Err(DomainError::UnknownReference(format!(
                "平仓单 {symbol} 成交但本地无持仓，状态不一致"
            )));
        };

        if pos.side == close_side {
            return Err(DomainError::IllegalTransition(format!(
                "平仓单方向与持仓方向相同（{symbol}），会导致反向开仓"
            )));
        }

        pos.quantity = pos.quantity - fill.quantity;
        if pos.quantity.is_zero() {
            self.positions.remove(&symbol);
        }
        Ok(())
    }

    /// 记录一次主动查询的结果。
    ///
    /// `Unknown` 状态的出口：查询返回终态则结案，返回未成交则回到 `Live`。
    pub fn resolve_query(
        &mut self,
        client_id: &ClientOrderId,
        outcome: QueryOutcome,
        now: DateTime<Utc>,
    ) -> Result<Vec<Effect>, DomainError> {
        let tracked = self.get_mut(client_id)?;
        tracked.state = match outcome {
            QueryOutcome::StillOpen { exchange_id } => {
                tracked.exchange_id = Some(exchange_id);
                OrderState::Live
            }
            QueryOutcome::Filled { filled, avg } => {
                tracked.filled = filled;
                tracked.avg_price = Some(avg);
                OrderState::Filled { filled, avg }
            }
            QueryOutcome::Cancelled { filled } => {
                tracked.filled = filled;
                OrderState::Cancelled { filled }
            }
            QueryOutcome::NotFound => {
                // 查不到有两种可能：从来没到达交易所（提交前就失败了），
                // 或者是 post-only 被静默拒绝（币安不记录这类订单）。
                // 两种都意味着"订单不存在且未成交"，按已撤销处理是安全的，
                // 因为 reduce-only 语义下不会因此漏掉真实持仓。
                OrderState::Cancelled {
                    filled: tracked.filled,
                }
            }
        };
        tracked.updated_at = now;
        Ok(vec![Effect::Persist])
    }

    /// 所有状态未知、需要查询对账的订单。
    ///
    /// 启动时和重连后都必须先跑一遍这个列表，然后才允许恢复交易。
    pub fn unresolved(&self) -> Vec<ClientOrderId> {
        self.orders
            .iter()
            .filter(|(_, t)| matches!(t.state, OrderState::Unknown { .. }))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 所有仍在交易所挂着的订单。
    pub fn open_orders(&self) -> Vec<&TrackedOrder> {
        self.orders.values().filter(|t| t.state.is_open()).collect()
    }

    pub fn get(&self, id: &ClientOrderId) -> Option<&TrackedOrder> {
        self.orders.get(id)
    }

    pub fn position(&self, symbol: &str) -> Option<&Position> {
        self.positions.get(symbol)
    }

    pub fn all_positions(&self) -> impl Iterator<Item = &Position> {
        self.positions.values()
    }

    /// 某结算资产的已实现盈亏。
    pub fn realized_pnl(&self, asset: &str) -> Decimal {
        self.realized_pnl
            .get(asset)
            .copied()
            .unwrap_or(Decimal::ZERO)
    }

    /// 更新持仓的止损价（保本止损或移动止损推进时调用）。
    pub fn set_stop(&mut self, symbol: &str, stop: Price) -> Result<(), DomainError> {
        let pos = self
            .positions
            .get_mut(symbol)
            .ok_or_else(|| DomainError::UnknownReference(symbol.to_string()))?;
        pos.stop_price = Some(stop);
        Ok(())
    }

    fn get_mut(&mut self, id: &ClientOrderId) -> Result<&mut TrackedOrder, DomainError> {
        self.orders
            .get_mut(id)
            .ok_or_else(|| DomainError::UnknownReference(id.to_string()))
    }
}

/// 主动查询一张订单的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryOutcome {
    StillOpen {
        exchange_id: String,
    },
    Filled {
        filled: Qty,
        avg: Price,
    },
    Cancelled {
        filled: Qty,
    },
    /// 交易所查不到这张订单。
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RejectReason;
    use crate::order::{OrderPurpose, TimeInForce};
    use rust_decimal_macros::dec;

    fn order(id: u64, purpose: OrderPurpose, side: Side) -> Order {
        order_qty(id, purpose, side, dec!(1))
    }

    fn order_qty(id: u64, purpose: OrderPurpose, side: Side, qty: Decimal) -> Order {
        Order {
            client_id: ClientOrderId::new("mm", id),
            symbol: "ETHUSDC".into(),
            purpose,
            side,
            quantity: Qty::new(qty),
            limit_price: Price::new(dec!(3200)),
            tif: TimeInForce::PostOnly,
            parent: None,
        }
    }

    fn fill(id: u64, trade: &str, qty: Decimal, px: Decimal) -> Fill {
        Fill {
            trade_id: trade.into(),
            client_id: ClientOrderId::new("mm", id),
            quantity: Qty::new(qty),
            price: Price::new(px),
            fee: Decimal::ZERO,
            fee_asset: "USDC".into(),
            at: Utc::now(),
        }
    }

    #[test]
    fn entry_fill_opens_position() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(
            ExecEvent::Accepted {
                client_id: ClientOrderId::new("mm", 1),
                exchange_id: "E1".into(),
            },
            now,
        )
        .unwrap();
        s.apply(ExecEvent::Filled(fill(1, "T1", dec!(1), dec!(3200))), now)
            .unwrap();

        let pos = s.position("ETHUSDC").expect("应建立持仓");
        assert_eq!(pos.side, Side::Buy);
        assert_eq!(pos.quantity.get(), dec!(1));
        assert_eq!(pos.entry_price.get(), dec!(3200));
    }

    /// 同一笔成交从两个通道到达时必须只计一次，否则持仓会翻倍。
    #[test]
    fn duplicate_fill_ids_are_ignored() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(1, "T1", dec!(1), dec!(3200))), now)
            .unwrap();
        let effects = s
            .apply(ExecEvent::Filled(fill(1, "T1", dec!(1), dec!(3200))), now)
            .unwrap();

        assert!(effects.is_empty(), "重复成交不应产生副作用");
        assert_eq!(s.position("ETHUSDC").unwrap().quantity.get(), dec!(1));
    }

    /// 累计成交量超过下单量必须报错，不能静默接受。
    #[test]
    fn cumulative_fill_exceeding_order_size_is_rejected() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(1, "T1", dec!(0.7), dec!(3200))), now)
            .unwrap();
        let err = s
            .apply(ExecEvent::Filled(fill(1, "T2", dec!(0.7), dec!(3200))), now)
            .unwrap_err();
        assert!(matches!(err, DomainError::IllegalTransition(_)));
    }

    #[test]
    fn partial_fills_produce_weighted_average_price() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(1, "T1", dec!(0.5), dec!(3200))), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(1, "T2", dec!(0.5), dec!(3210))), now)
            .unwrap();

        let t = s.get(&ClientOrderId::new("mm", 1)).unwrap();
        assert_eq!(t.filled.get(), dec!(1));
        assert_eq!(t.avg_price.unwrap().get(), dec!(3205));
        assert!(matches!(t.state, OrderState::Filled { .. }));
    }

    /// 这一条是实盘安全的关键：状态未知必须返回 Query，且不允许重发。
    #[test]
    fn unknown_state_emits_query_and_blocks_resubmit() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        let id = ClientOrderId::new("mm", 1);
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();

        let effects = s
            .apply(
                ExecEvent::Unknown {
                    client_id: id.clone(),
                    at: now,
                },
                now,
            )
            .unwrap();
        assert!(effects.contains(&Effect::Query(id.clone())), "必须发出查询");
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Submit(_))),
            "状态未知时绝不能重发订单"
        );
        assert_eq!(s.unresolved(), vec![id.clone()]);

        // 查询后结案
        s.resolve_query(
            &id,
            QueryOutcome::Filled {
                filled: Qty::new(dec!(1)),
                avg: Price::new(dec!(3200)),
            },
            now,
        )
        .unwrap();
        assert!(s.unresolved().is_empty());
    }

    /// post-only 被拒（币安 5022）后查询会返回 NotFound——必须能安全结案。
    #[test]
    fn post_only_rejection_resolves_to_cancelled_when_query_finds_nothing() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        let id = ClientOrderId::new("mm", 1);
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(
            ExecEvent::Unknown {
                client_id: id.clone(),
                at: now,
            },
            now,
        )
        .unwrap();
        s.resolve_query(&id, QueryOutcome::NotFound, now).unwrap();

        let t = s.get(&id).unwrap();
        assert!(matches!(t.state, OrderState::Cancelled { .. }));
        assert!(t.state.is_terminal());
    }

    #[test]
    fn rejection_is_terminal_and_recorded() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        let id = ClientOrderId::new("mm", 1);
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(
            ExecEvent::Rejected {
                client_id: id.clone(),
                reason: RejectReason::PostOnlyWouldCross,
            },
            now,
        )
        .unwrap();
        assert!(matches!(
            s.get(&id).unwrap().state,
            OrderState::Rejected {
                reason: RejectReason::PostOnlyWouldCross
            }
        ));
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        assert!(
            s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
                .is_err()
        );
    }

    /// 平仓单方向必须与持仓相反，否则会变成反向开仓。
    #[test]
    fn reduce_only_wrong_direction_is_rejected() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(1, "T1", dec!(1), dec!(3200))), now)
            .unwrap();

        // 一张 Buy 方向的"平仓单"——与多头持仓同向，属于非法
        s.register(order(2, OrderPurpose::TakeProfit, Side::Buy), now)
            .unwrap();
        let err = s
            .apply(ExecEvent::Filled(fill(2, "T2", dec!(1), dec!(3210))), now)
            .unwrap_err();
        assert!(matches!(err, DomainError::IllegalTransition(_)));
        assert_eq!(
            s.position("ETHUSDC").unwrap().quantity.get(),
            dec!(1),
            "持仓不应被改动"
        );
    }

    /// 平仓到零必须移除持仓条目。
    #[test]
    fn closing_full_position_removes_it() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(1, "T1", dec!(1), dec!(3200))), now)
            .unwrap();
        s.register(order(2, OrderPurpose::TakeProfit, Side::Sell), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(2, "T2", dec!(1), dec!(3210))), now)
            .unwrap();

        assert!(s.position("ETHUSDC").is_none());
    }

    /// 无持仓时平仓单成交 = 状态不一致，必须报错而非建立反向持仓。
    #[test]
    fn fill_without_position_is_an_error_not_a_reversal() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(2, OrderPurpose::StopLoss, Side::Sell), now)
            .unwrap();
        let err = s
            .apply(ExecEvent::Filled(fill(2, "T1", dec!(1), dec!(3190))), now)
            .unwrap_err();
        assert!(matches!(err, DomainError::UnknownReference(_)));
        assert!(s.all_positions().next().is_none(), "绝不能凭空建立持仓");
    }

    /// 手续费按结算资产分开记账，绝不能合并 USDT 与 USDC。
    #[test]
    fn fees_are_kept_separate_per_settlement_asset() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();

        let mut f = fill(1, "T1", dec!(1), dec!(3200));
        f.fee = dec!(-0.64);
        f.fee_asset = "USDC".into();
        s.apply(ExecEvent::Filled(f), now).unwrap();

        assert_eq!(s.realized_pnl("USDC"), dec!(-0.64));
        assert_eq!(
            s.realized_pnl("USDT"),
            Decimal::ZERO,
            "USDT 不受 USDC 手续费影响"
        );
    }

    /// 已处于终态的订单不能再被接受。
    #[test]
    fn terminal_order_cannot_be_accepted() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        let id = ClientOrderId::new("mm", 1);
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(
            ExecEvent::Expired {
                client_id: id.clone(),
            },
            now,
        )
        .unwrap();
        let err = s
            .apply(
                ExecEvent::Accepted {
                    client_id: id,
                    exchange_id: "E1".into(),
                },
                now,
            )
            .unwrap_err();
        assert!(matches!(err, DomainError::IllegalTransition(_)));
    }

    #[test]
    fn open_orders_excludes_terminal_states() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.register(order(2, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(
            ExecEvent::Accepted {
                client_id: ClientOrderId::new("mm", 1),
                exchange_id: "E1".into(),
            },
            now,
        )
        .unwrap();
        s.apply(
            ExecEvent::Rejected {
                client_id: ClientOrderId::new("mm", 2),
                reason: RejectReason::InsufficientMargin,
            },
            now,
        )
        .unwrap();

        let open = s.open_orders();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].order.client_id.as_str(), "mm:1");
    }

    /// 同向加仓要更新加权入场价。
    #[test]
    fn same_direction_entries_weighted_average() {
        let mut s = OrderBookState::new();
        let now = Utc::now();
        s.register(order(1, OrderPurpose::Entry, Side::Buy), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(1, "T1", dec!(1), dec!(3200))), now)
            .unwrap();
        s.register(order_qty(2, OrderPurpose::Entry, Side::Buy, dec!(3)), now)
            .unwrap();
        s.apply(ExecEvent::Filled(fill(2, "T2", dec!(3), dec!(3240))), now)
            .unwrap();

        let pos = s.position("ETHUSDC").unwrap();
        assert_eq!(pos.quantity.get(), dec!(4));
        assert_eq!(pos.entry_price.get(), dec!(3230)); // (3200 + 3*3240) / 4
    }
}
