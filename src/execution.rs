use rust_decimal::Decimal;

use crate::model::{Side, quantize_down};

/// 订单用途。执行层不提供市价订单类型，所有意图都必须是限价单。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrderPurpose {
    Entry,
    TakeProfit,
    StopLoss,
}

impl OrderPurpose {
    pub fn is_reduce_only(self) -> bool {
        matches!(self, Self::TakeProfit | Self::StopLoss)
    }
}

/// 经过策略和风险检查后交给执行器的 Maker 限价订单。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MakerOrder {
    pub client_order_id: String,
    pub purpose: OrderPurpose,
    pub side: Side,
    pub quantity: Decimal,
    pub price: Decimal,
}

impl MakerOrder {
    pub fn validate(
        &self,
        tick_size: Decimal,
        step_size: Decimal,
        min_qty: Decimal,
        min_notional: Decimal,
    ) -> Result<(), MakerOrderError> {
        if self.client_order_id.trim().is_empty() {
            return Err(MakerOrderError::MissingClientOrderId);
        }
        if self.quantity <= Decimal::ZERO || self.price <= Decimal::ZERO {
            return Err(MakerOrderError::NonPositiveValue);
        }
        if tick_size <= Decimal::ZERO || step_size <= Decimal::ZERO {
            return Err(MakerOrderError::InvalidTradingRule);
        }
        if quantize_down(self.price, tick_size) != self.price {
            return Err(MakerOrderError::PricePrecision);
        }
        if quantize_down(self.quantity, step_size) != self.quantity {
            return Err(MakerOrderError::QuantityPrecision);
        }
        if self.quantity < min_qty {
            return Err(MakerOrderError::MinimumQuantity);
        }
        if self.quantity * self.price < min_notional {
            return Err(MakerOrderError::MinimumNotional);
        }
        Ok(())
    }

    pub fn is_reduce_only(&self) -> bool {
        self.purpose.is_reduce_only()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MakerOrderError {
    MissingClientOrderId,
    NonPositiveValue,
    InvalidTradingRule,
    PricePrecision,
    QuantityPrecision,
    MinimumQuantity,
    MinimumNotional,
}

impl std::fmt::Display for MakerOrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::MissingClientOrderId => "Maker 订单缺少客户端唯一标识",
            Self::NonPositiveValue => "Maker 订单价格和数量必须为正数",
            Self::InvalidTradingRule => "交易规则精度必须为正数",
            Self::PricePrecision => "Maker 订单价格不符合 tickSize",
            Self::QuantityPrecision => "Maker 订单数量不符合 stepSize",
            Self::MinimumQuantity => "Maker 订单数量低于 minQty",
            Self::MinimumNotional => "Maker 订单名义价值低于 minNotional",
        };
        f.write_str(message)
    }
}

impl std::error::Error for MakerOrderError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(purpose: OrderPurpose) -> MakerOrder {
        MakerOrder {
            client_order_id: "mm-entry-1".to_string(),
            purpose,
            side: Side::Buy,
            quantity: Decimal::new(5, 2),
            price: Decimal::from(100),
        }
    }

    #[test]
    fn accepts_quantized_maker_order_and_marks_exits_reduce_only() {
        assert!(
            order(OrderPurpose::Entry)
                .validate(
                    Decimal::ONE,
                    Decimal::new(1, 3),
                    Decimal::new(1, 3),
                    Decimal::from(5)
                )
                .is_ok()
        );
        assert!(!order(OrderPurpose::Entry).is_reduce_only());
        assert!(order(OrderPurpose::TakeProfit).is_reduce_only());
        assert!(order(OrderPurpose::StopLoss).is_reduce_only());
    }

    #[test]
    fn rejects_unquantized_or_too_small_orders() {
        let mut invalid = order(OrderPurpose::Entry);
        invalid.price = Decimal::new(1001, 1);
        assert_eq!(
            invalid.validate(
                Decimal::ONE,
                Decimal::new(1, 3),
                Decimal::new(1, 3),
                Decimal::from(5)
            ),
            Err(MakerOrderError::PricePrecision)
        );

        let mut too_small = order(OrderPurpose::Entry);
        too_small.quantity = Decimal::new(1, 3);
        assert_eq!(
            too_small.validate(
                Decimal::ONE,
                Decimal::new(1, 3),
                Decimal::new(1, 3),
                Decimal::from(5)
            ),
            Err(MakerOrderError::MinimumNotional)
        );
    }
}
