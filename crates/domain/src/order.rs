//! 订单、订单状态机、以及状态机产出的副作用。
//!
//! 状态机**从不做 I/O**：它接收事件、返回 `Effect` 列表，由编排层去执行。
//! 这条边界让回测/模拟盘/实盘共用同一套生命周期，也让全部迁移可用纯函数测试。

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::RejectReason;
use crate::money::{Price, Qty};

/// 买卖方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub const fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

/// 订单用途。区分开仓与平仓，因为两者在风控和 reduce-only 语义上不同。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderPurpose {
    /// 开仓。必须是 post-only。
    Entry,
    /// 止盈。必须是 reduce-only 的 post-only。
    TakeProfit,
    /// 止损。必须是 reduce-only 的 post-only。
    StopLoss,
}

impl OrderPurpose {
    /// 平仓类订单必须带 reduce-only，否则可能反向开仓。
    pub const fn is_reduce_only(self) -> bool {
        matches!(self, OrderPurpose::TakeProfit | OrderPurpose::StopLoss)
    }

    pub const fn is_entry(self) -> bool {
        matches!(self, OrderPurpose::Entry)
    }
}

/// 客户端订单 ID。本系统自己生成，是订单的唯一可信标识。
///
/// 为什么不用交易所返回的 `orderId` 作为主键：post-only 被拒时交易所
/// 不返回任何可查询记录（见 `RejectReason::PostOnlyWouldCross`），只有
/// 我们提交时带的 `clientOrderId` 能贯穿全链路。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClientOrderId(pub String);

impl ClientOrderId {
    pub fn new(prefix: &str, seq: u64) -> Self {
        Self(format!("{prefix}:{seq}"))
    }

    /// 派生一张子订单 ID（例如某张入场单的止损单）。
    ///
    /// 旧实现在这里出过 bug：止盈/止损用 `format!("{}:tp", id)` 拼接，
    /// 而入场 ID 里硬编码了 `"retest"` 策略名，导致同一秒确认的缠论信号
    /// 和回踩信号生成相同的 client order id 而在实盘撞单。这里要求子 ID
    /// 必须来自真实的父 ID，且不再有策略名字面量。
    pub fn child(&self, tag: &str) -> Self {
        Self(format!("{}:{tag}", self.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ClientOrderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 时效指令。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimeInForce {
    /// GTX：只做 maker，会立即成交则被拒。本系统**唯一**允许的开仓时效。
    PostOnly,
    /// GTX + 到期自动过期。交易所侧兜底"挂出去 N 分钟没人吃就撤"，
    /// 进程崩溃也生效。
    PostOnlyGtd { deadline: DateTime<Utc> },
}

/// 一张订单。注意：**没有 `status` 字段**——状态只存在于状态机里，
/// 避免出现"订单自己声称的状态"和"状态机认为的状态"两个真相。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub client_id: ClientOrderId,
    pub symbol: String,
    pub purpose: OrderPurpose,
    pub side: Side,
    pub quantity: Qty,
    pub limit_price: Price,
    pub tif: TimeInForce,
    /// 平仓单指向其对应的入场单。开仓单为 `None`。
    pub parent: Option<ClientOrderId>,
}

impl Order {
    pub fn reduce_only(&self) -> bool {
        self.purpose.is_reduce_only()
    }

    /// 订单在 `now` 是否已过期（仅对 GTD 有意义）。
    pub fn expired_at(&self, now: DateTime<Utc>) -> bool {
        match self.tif {
            TimeInForce::PostOnly => false,
            TimeInForce::PostOnlyGtd { deadline } => now >= deadline,
        }
    }
}

/// 订单生命周期。三种模式共用这一套。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderState {
    /// 已生成意图，尚未提交。
    PendingSubmit,
    /// 已提交，交易所已确认在挂。
    Live,
    /// 部分成交。maker 单在小额挂单上少见，但并非不可能。
    PartiallyFilled { filled: Qty, avg: Price },
    /// 全部成交。
    Filled { filled: Qty, avg: Price },
    /// 已撤销，可能带部分成交。
    Cancelled { filled: Qty },
    /// 被拒绝。
    Rejected { reason: RejectReason },
    /// GTD 到期自动过期。
    Expired,
    /// **状态未知，必须先查询对账。**
    ///
    /// 不是错误状态，而是一个要求：在 `since` 时刻我们发出过一个请求但
    /// 没得到确定的响应。在查清之前禁止重发同 ID 的订单。
    Unknown {
        since: DateTime<Utc>,
        last_probe: Option<DateTime<Utc>>,
    },
}

impl OrderState {
    /// 终态：不会再变化。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrderState::Filled { .. }
                | OrderState::Cancelled { .. }
                | OrderState::Rejected { .. }
                | OrderState::Expired
        )
    }

    /// 仍在交易所挂着的状态。
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            OrderState::Live | OrderState::PartiallyFilled { .. } | OrderState::Unknown { .. }
        )
    }

    pub fn filled_quantity(&self) -> Qty {
        match self {
            OrderState::PartiallyFilled { filled, .. } | OrderState::Filled { filled, .. } => {
                *filled
            }
            OrderState::Cancelled { filled } => *filled,
            _ => Qty::ZERO,
        }
    }
}

/// 交易暂停原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HaltReason {
    /// 行情陈旧，禁止开仓。已有保护单继续管理。
    StaleFeed,
    /// 行情断线。
    FeedDisconnected,
    /// 账户与远端不一致。
    PositionMismatch,
    /// 出现无法对账的订单。
    UnresolvedOrders,
    /// 达到风控限额。
    RiskLimit,
    /// 操作员手动停止。
    OperatorKill,
}

/// 状态机要求编排层执行的副作用。
///
/// 状态机不执行这些动作，只声明它们——这是"回测与实盘共用一套逻辑"的关键，
/// 因为回测只需实现这些 Effect 的模拟版本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Submit(Box<Order>),
    Cancel(ClientOrderId),
    /// 查询一张状态未知的订单。**这是 `Unknown` 状态的唯一出口。**
    Query(ClientOrderId),
    /// 热状态已变更，需要持久化。
    Persist,
    /// 停止交易。
    Halt(HaltReason),
    /// 需要人工关注的告警（不改变状态）。
    Alarm(Alarm),
}

/// 告警。全部面向操作者可见，不允许静默失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alarm {
    /// 止损限价单未能成交，仓位裸露中。
    ///
    /// 这是 maker-only 特有的风险：止损也是挂单，价格跳空穿过它且不回来时
    /// 没有任何机制会平掉仓位。必须让操作者看到。
    StopUnfilledExposure {
        order: ClientOrderId,
        since: DateTime<Utc>,
    },
    /// 订单状态长时间无法对账。
    ReconcileTimeout {
        order: ClientOrderId,
        since: DateTime<Utc>,
    },
    /// 费率发生变化，可能与回测假设不符。整个 edge 依赖零费率活动。
    FeeDrift { old: Decimal, new: Decimal },
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn only_take_profit_and_stop_loss_are_reduce_only() {
        assert!(OrderPurpose::TakeProfit.is_reduce_only());
        assert!(OrderPurpose::StopLoss.is_reduce_only());
        assert!(!OrderPurpose::Entry.is_reduce_only());
    }

    #[test]
    fn derived_child_ids_are_unique_per_parent() {
        let a = ClientOrderId::new("mm", 1);
        let b = ClientOrderId::new("mm", 2);
        assert_ne!(a.child("tp"), b.child("tp"));
        assert_eq!(a.child("tp").as_str(), "mm:1:tp");
    }

    /// 旧实现把策略名硬编码进订单 ID，导致不同策略在同一秒撞单。
    /// 新实现里策略不参与 ID 构造，这个测试锁住这一点。
    #[test]
    fn order_id_does_not_encode_strategy_name() {
        let id = ClientOrderId::new("mm", 42);
        assert!(!id.as_str().contains("retest"));
        assert!(!id.as_str().contains("chan"));
    }

    #[test]
    fn terminal_states_are_recognized() {
        assert!(
            OrderState::Filled {
                filled: Qty::new(dec!(1)),
                avg: Price::new(dec!(100))
            }
            .is_terminal()
        );
        assert!(OrderState::Expired.is_terminal());
        assert!(!OrderState::Live.is_terminal());
        assert!(
            !OrderState::Unknown {
                since: Utc::now(),
                last_probe: None
            }
            .is_terminal()
        );
    }

    /// `Unknown` 必须算作"仍开着"，否则对账流程会把它当已结束而漏掉。
    #[test]
    fn unknown_state_counts_as_open_for_reconciliation() {
        let unknown = OrderState::Unknown {
            since: Utc::now(),
            last_probe: None,
        };
        assert!(unknown.is_open());
    }

    #[test]
    fn gtd_expiry_is_respected() {
        let now = Utc::now();
        let order = Order {
            client_id: ClientOrderId::new("mm", 1),
            symbol: "ETHUSDC".into(),
            purpose: OrderPurpose::Entry,
            side: Side::Buy,
            quantity: Qty::new(dec!(1)),
            limit_price: Price::new(dec!(3200)),
            tif: TimeInForce::PostOnlyGtd { deadline: now },
            parent: None,
        };
        assert!(order.expired_at(now));
        assert!(!order.expired_at(now - chrono::Duration::seconds(1)));
    }

    #[test]
    fn plain_post_only_never_expires() {
        let now = Utc::now();
        let order = Order {
            client_id: ClientOrderId::new("mm", 1),
            symbol: "ETHUSDC".into(),
            purpose: OrderPurpose::Entry,
            side: Side::Buy,
            quantity: Qty::new(dec!(1)),
            limit_price: Price::new(dec!(3200)),
            tif: TimeInForce::PostOnly,
            parent: None,
        };
        assert!(!order.expired_at(now + chrono::Duration::days(365)));
    }
}
