use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

use crate::user_stream::OrderTradeUpdate;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteOrderState {
    PendingSubmit,
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Expired,
    Unknown,
}

impl RemoteOrderState {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Canceled | Self::Rejected | Self::Expired
        )
    }
}

#[derive(Clone, Debug)]
pub struct TrackedOrder {
    pub client_order_id: String,
    pub exchange_order_id: Option<String>,
    pub expected_quantity: Decimal,
    pub filled_quantity: Decimal,
    pub state: RemoteOrderState,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileAction {
    QueryOrder,
    MarkFilled,
    MarkCanceled,
    MarkRejected,
    Alert,
}

pub struct OrderReconciler {
    symbol: String,
    orders: BTreeMap<String, TrackedOrder>,
    seen_events: std::collections::HashSet<String>,
}

impl OrderReconciler {
    pub fn new(symbol: String) -> Self {
        Self {
            symbol,
            orders: BTreeMap::new(),
            seen_events: std::collections::HashSet::new(),
        }
    }

    pub fn register(
        &mut self,
        client_order_id: String,
        expected_quantity: Decimal,
        now: DateTime<Utc>,
    ) -> Result<(), &'static str> {
        if client_order_id.trim().is_empty() || expected_quantity <= Decimal::ZERO {
            return Err("订单标识和预期数量必须有效");
        }
        if self.orders.contains_key(&client_order_id) {
            return Err("客户端订单标识已存在，拒绝重复登记");
        }
        self.orders.insert(
            client_order_id.clone(),
            TrackedOrder {
                client_order_id,
                exchange_order_id: None,
                expected_quantity,
                filled_quantity: Decimal::ZERO,
                state: RemoteOrderState::PendingSubmit,
                updated_at: now,
            },
        );
        Ok(())
    }

    pub fn submit_ack(
        &mut self,
        client_order_id: &str,
        exchange_order_id: String,
        now: DateTime<Utc>,
    ) -> Result<(), &'static str> {
        let order = self
            .orders
            .get_mut(client_order_id)
            .ok_or("未知客户端订单，拒绝确认")?;
        if order.state != RemoteOrderState::PendingSubmit {
            return Err("订单不在待提交状态");
        }
        order.exchange_order_id = Some(exchange_order_id);
        order.state = RemoteOrderState::New;
        order.updated_at = now;
        Ok(())
    }

    pub fn submit_unknown(&mut self, client_order_id: &str, now: DateTime<Utc>) -> ReconcileAction {
        if let Some(order) = self.orders.get_mut(client_order_id) {
            order.state = RemoteOrderState::Unknown;
            order.updated_at = now;
        }
        ReconcileAction::QueryOrder
    }

    pub fn on_reconnect(&self) -> Vec<ReconcileAction> {
        self.orders
            .values()
            .filter(|order| !order.state.terminal())
            .map(|_| ReconcileAction::QueryOrder)
            .collect()
    }

    pub fn apply_event(
        &mut self,
        event: &OrderTradeUpdate,
    ) -> Result<Option<ReconcileAction>, &'static str> {
        if event.symbol != self.symbol {
            return Ok(None);
        }
        let event_key = format!(
            "{}:{}:{}",
            event.exchange_order_id,
            event.last_trade_id.as_deref().unwrap_or("event"),
            event.execution_type
        );
        if !self.seen_events.insert(event_key) {
            return Ok(None);
        }
        let order = self
            .orders
            .get_mut(&event.client_order_id)
            .ok_or("收到未登记订单事件，进入人工检查")?;
        if order
            .exchange_order_id
            .as_ref()
            .is_some_and(|id| id != &event.exchange_order_id)
        {
            order.state = RemoteOrderState::Unknown;
            return Ok(Some(ReconcileAction::Alert));
        }
        order.exchange_order_id = Some(event.exchange_order_id.clone());
        if event.cumulative_filled_quantity > order.expected_quantity {
            order.state = RemoteOrderState::Unknown;
            return Ok(Some(ReconcileAction::Alert));
        }
        order.filled_quantity = event.cumulative_filled_quantity;
        order.updated_at = event.event_time;
        order.state = match event.status.as_str() {
            "NEW" => RemoteOrderState::New,
            "PARTIALLY_FILLED" => RemoteOrderState::PartiallyFilled,
            "FILLED" => RemoteOrderState::Filled,
            "CANCELED" => RemoteOrderState::Canceled,
            "REJECTED" => RemoteOrderState::Rejected,
            "EXPIRED" => RemoteOrderState::Expired,
            _ => {
                order.state = RemoteOrderState::Unknown;
                return Ok(Some(ReconcileAction::Alert));
            }
        };
        Ok(Some(match order.state {
            RemoteOrderState::Filled => ReconcileAction::MarkFilled,
            RemoteOrderState::Canceled | RemoteOrderState::Expired => ReconcileAction::MarkCanceled,
            RemoteOrderState::Rejected => ReconcileAction::MarkRejected,
            _ => ReconcileAction::QueryOrder,
        }))
    }

    pub fn get(&self, client_order_id: &str) -> Option<&TrackedOrder> {
        self.orders.get(client_order_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_stream::parse_order_trade_update;
    use chrono::TimeZone;

    const UPDATE: &str = r#"{
      "e":"ORDER_TRADE_UPDATE","E":1727000000123,
      "o":{"s":"ETHUSDC","c":"mm-entry-1","i":"12345","X":"FILLED","x":"TRADE","t":88,"l":"0.050","z":"0.050","ap":"100.00","R":false}
    }"#;

    #[test]
    fn unknown_submit_requires_query_and_event_closes_order() {
        let now = Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0).unwrap();
        let mut reconciler = OrderReconciler::new("ETHUSDC".to_string());
        reconciler
            .register("mm-entry-1".to_string(), Decimal::new(50, 3), now)
            .unwrap();
        assert_eq!(
            reconciler.submit_unknown("mm-entry-1", now),
            ReconcileAction::QueryOrder
        );
        let event = parse_order_trade_update(UPDATE).unwrap().unwrap();
        assert_eq!(
            reconciler.apply_event(&event).unwrap(),
            Some(ReconcileAction::MarkFilled)
        );
        assert_eq!(
            reconciler.get("mm-entry-1").unwrap().state,
            RemoteOrderState::Filled
        );
        assert!(reconciler.on_reconnect().is_empty());
    }

    #[test]
    fn duplicate_registration_and_mismatched_exchange_id_are_safe() {
        let now = Utc::now();
        let mut reconciler = OrderReconciler::new("ETHUSDC".to_string());
        reconciler
            .register("mm-entry-1".to_string(), Decimal::ONE, now)
            .unwrap();
        assert!(
            reconciler
                .register("mm-entry-1".to_string(), Decimal::ONE, now)
                .is_err()
        );
        reconciler
            .submit_ack("mm-entry-1", "first".to_string(), now)
            .unwrap();
        let mut event = parse_order_trade_update(UPDATE).unwrap().unwrap();
        event.exchange_order_id = "second".to_string();
        assert_eq!(
            reconciler.apply_event(&event).unwrap(),
            Some(ReconcileAction::Alert)
        );
        assert_eq!(
            reconciler.get("mm-entry-1").unwrap().state,
            RemoteOrderState::Unknown
        );
    }
}
