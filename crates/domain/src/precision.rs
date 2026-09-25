//! 价格与数量的量化（对齐交易所 tick / step）。
//!
//! # 为什么需要 `PriceRole`
//!
//! 旧实现在三个地方各自手选了舍入方向，并且**已经分叉**：
//!
//! - `backtest.rs` 对多空两侧的止盈都用向下取整
//! - `paper.rs` / `live.rs` 对买入止盈用向上取整
//!
//! 后果是回测在一个实盘永远不会挂出的价格上成交多单止盈。止盈目标是 bp 级
//! 时，这个误差足以让回测结论失效，而且没有任何测试会发现。
//!
//! 修法不是"记得选对"，而是**取消手选权**：调用方只声明这个价格的用途
//! （`PriceRole`），舍入方向由本模块唯一决定。

use rust_decimal::Decimal;

use crate::error::DomainError;
use crate::money::{Price, Qty};
use crate::order::Side;

/// 这个价格在策略里扮演什么角色。决定舍入方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PriceRole {
    /// 被动挂单的入场价：允许的范围内**尽量贴近市价**以争取成交，
    /// 但绝不能穿过对手价，否则 post-only 会被拒（币安错误码 5022）。
    PassiveEntry,
    /// 止盈价：必须挂在**比目标更远**的一侧，宁可少赚也不能变成吃单。
    TakeProfit,
    /// 止损价：必须挂在**离市价更近**的一侧，让它尽可能早触发。
    ///
    /// 注意 maker-only 下止损是挂单，可能不成交——所以"更容易触发"是有价值
    /// 的：多一分成交机会，少一分裸露风险。
    StopLoss,
}

/// 交易所的精度规则。来自 `exchangeInfo` 的 `PRICE_FILTER` / `LOT_SIZE` /
/// `MIN_NOTIONAL` 过滤器。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Precision {
    #[serde(with = "rust_decimal::serde::str")]
    pub tick_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub step_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_notional: Decimal,
}

impl Precision {
    /// 把原始价格对齐到 tick，方向由 `order_side` + `role` 唯一决定。
    ///
    /// `order_side` 是**这张订单本身的方向**，不是持仓方向。平仓单的方向
    /// 与持仓相反，调用方不要自己做转换——`ProtectionPlanner` 已经传入了
    /// 正确的出场单方向。
    ///
    /// # 契约
    ///
    /// 全部按"订单方向"表达，且每种组合都是刻意选的：
    ///
    /// | role \ 订单方向 | Buy | Sell |
    /// |---|---|---|
    /// | `PassiveEntry` | 向下——买价更低才不会吃卖单 | 向上——卖价更高才不会吃买单 |
    /// | `TakeProfit` | 向下——买方止盈（平空）越便宜越好 | 向上——卖方止盈（平多）越贵越好 |
    /// | `StopLoss` | 向上——买方止损（平空）越贵越早触发 | 向下——卖方止损（平多）越低越早触发 |
    ///
    /// `TakeProfit` 与 `StopLoss` 在同一方向上是**相反**的取整，这不是笔误：
    /// 止盈要"多赚一跳"，止损要"早出一跳"，两者目标相反。
    pub fn price_for(
        &self,
        order_side: Side,
        raw: Decimal,
        role: PriceRole,
    ) -> Result<Price, DomainError> {
        if raw <= Decimal::ZERO {
            return Err(DomainError::NonPositivePrice(raw));
        }
        if self.tick_size <= Decimal::ZERO {
            return Err(DomainError::InvalidTickSize(self.tick_size));
        }

        let round_up = match (role, order_side) {
            // 被动挂单：远离对手价，避免 post-only 被拒
            (PriceRole::PassiveEntry, Side::Buy) => false,
            (PriceRole::PassiveEntry, Side::Sell) => true,
            // 止盈：买方（平空）压低价，卖方（平多）抬高价 -> 都是"多赚一跳"
            (PriceRole::TakeProfit, Side::Buy) => false,
            (PriceRole::TakeProfit, Side::Sell) => true,
            // 止损：买方（平空）抬高价，卖方（平多）压低价 -> 都是"早出一跳"
            (PriceRole::StopLoss, Side::Buy) => true,
            (PriceRole::StopLoss, Side::Sell) => false,
        };

        let stepped = if round_up {
            quantize_up(raw, self.tick_size)
        } else {
            quantize_down(raw, self.tick_size)
        };

        if stepped <= Decimal::ZERO {
            // 向下取整到 0 说明 tick 比价格还大，属于合约元数据异常。
            return Err(DomainError::QuantizedToZero {
                raw,
                tick: self.tick_size,
            });
        }
        Ok(Price::new(stepped))
    }

    /// 把原始数量按 step 向下取整。数量永远向下——宁可少成交也不能超量下单。
    pub fn quantity(&self, raw: Decimal) -> Result<Qty, DomainError> {
        if raw <= Decimal::ZERO {
            return Err(DomainError::NonPositiveQuantity(raw));
        }
        if self.step_size <= Decimal::ZERO {
            return Err(DomainError::InvalidStepSize(self.step_size));
        }
        let stepped = quantize_down(raw, self.step_size);
        if stepped <= Decimal::ZERO {
            return Err(DomainError::QuantityBelowStep {
                raw,
                step: self.step_size,
            });
        }
        Ok(Qty::new(stepped))
    }

    /// 量化数量，允许结果为 0。
    ///
    /// 用于分批止盈这类"某档算出来太小、应当跳过"的场景。返回 `Qty::ZERO`
    /// 表示该档位低于交易所 step，调用方应跳过而不是报错中止整个计划。
    ///
    /// 真正的非法输入（负数、step 配置错误）仍然报错——只有"小到不合法"被
    /// 降级为 0。
    pub fn quantity_or_zero(&self, raw: Decimal) -> Result<Qty, DomainError> {
        if raw < Decimal::ZERO {
            return Err(DomainError::NonPositiveQuantity(raw));
        }
        if self.step_size <= Decimal::ZERO {
            return Err(DomainError::InvalidStepSize(self.step_size));
        }
        Ok(Qty::new(quantize_down(raw, self.step_size)))
    }

    /// 校验数量是否满足 minQty 与 minNotional。
    pub fn check_order_size(&self, qty: Qty, price: Price) -> Result<(), DomainError> {
        if qty.get() < self.min_qty {
            return Err(DomainError::BelowMinQty {
                qty: qty.get(),
                min: self.min_qty,
            });
        }
        let notional = qty.get() * price.get();
        if notional < self.min_notional {
            return Err(DomainError::BelowMinNotional {
                notional,
                min: self.min_notional,
            });
        }
        Ok(())
    }
}

/// 向下取整到最近的 `step` 倍数。
///
/// 先除后乘而不是 `%` 取模：`Decimal` 的除法会保留 28 位有效数字，而乘法
/// 按操作数精度截断。对 `3200 × 1.00004` 这类结果，乘法会得到
/// `3200.1279999...` 而在 tick 边界上取整到错误的一侧。
pub fn quantize_down(value: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return value;
    }
    (value / step).floor() * step
}

/// 向上取整到最近的 `step` 倍数。先除后乘，理由同 `quantize_down`。
pub fn quantize_up(value: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return value;
    }
    (value / step).ceil() * step
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn prec() -> Precision {
        Precision {
            tick_size: dec!(0.01),
            step_size: dec!(0.001),
            min_qty: dec!(0.001),
            min_notional: dec!(5),
        }
    }

    /// 这个测试对应旧实现失效的场景。旧 `backtest.rs` 对多空两侧的止盈都用
    /// 向下取整，而 `paper.rs`/`live.rs` 对卖方（平多）止盈用向上取整。
    /// 平多止盈是**卖单**，必须向上取整才能多赚一跳。
    #[test]
    fn sell_take_profit_rounds_up_never_down() {
        let p = prec();
        let got = p
            .price_for(Side::Sell, dec!(3200.005), PriceRole::TakeProfit)
            .unwrap();
        assert_eq!(got.get(), dec!(3200.01), "平多止盈（卖单）必须向上取整");
    }

    /// 平空止盈是**买单**，向下取整才能买得更便宜。
    #[test]
    fn buy_take_profit_rounds_down() {
        let p = prec();
        let got = p
            .price_for(Side::Buy, dec!(3200.005), PriceRole::TakeProfit)
            .unwrap();
        assert_eq!(got.get(), dec!(3200.00), "平空止盈（买单）必须向下取整");
    }

    /// 被动入场价不能穿过对手价，否则 post-only 被拒（5022）。
    #[test]
    fn passive_entry_rounds_away_from_crossing() {
        let p = prec();
        let buy = p
            .price_for(Side::Buy, dec!(3200.005), PriceRole::PassiveEntry)
            .unwrap();
        assert_eq!(buy.get(), dec!(3200.00), "买价向下取整，避免吃卖单");
        let sell = p
            .price_for(Side::Sell, dec!(3200.005), PriceRole::PassiveEntry)
            .unwrap();
        assert_eq!(sell.get(), dec!(3200.01), "卖价向上取整，避免吃买单");
    }

    /// 止损要更容易触发：卖方向（平多）压低价，买方向（平空）抬高价。
    #[test]
    fn stop_loss_rounds_toward_triggering() {
        let p = prec();
        // 平多止损：卖单挂更低 -> 3190.00
        let close_long = p
            .price_for(Side::Sell, dec!(3190.004), PriceRole::StopLoss)
            .unwrap();
        assert_eq!(close_long.get(), dec!(3190.00), "平多止损压低价，更早触发");
        // 平空止损：买单挂更高 -> 3210.01
        let close_short = p
            .price_for(Side::Buy, dec!(3210.006), PriceRole::StopLoss)
            .unwrap();
        assert_eq!(close_short.get(), dec!(3210.01), "平空止损抬高价，更早触发");
    }

    /// 同一方向上止盈与止损的取整必须相反——止盈要多赚一跳，止损要早出一跳。
    #[test]
    fn take_profit_and_stop_loss_round_opposite_ways() {
        let p = prec();
        let raw = dec!(3190.004);
        let tp = p.price_for(Side::Sell, raw, PriceRole::TakeProfit).unwrap();
        let sl = p.price_for(Side::Sell, raw, PriceRole::StopLoss).unwrap();
        assert!(
            tp.get() > sl.get(),
            "卖方止盈价应高于止损价：tp={tp} sl={sl}"
        );
    }

    #[test]
    fn quantity_always_rounds_down() {
        let p = prec();
        assert_eq!(p.quantity(dec!(1.2349)).unwrap().get(), dec!(1.234));
        // 低于一个 step 的数量是明确的错误（调用方不该悄悄下单）
        assert!(matches!(
            p.quantity(dec!(0.0009)).unwrap_err(),
            DomainError::QuantityBelowStep { .. }
        ));
    }

    /// 分批止盈的过小档位走 `quantity_or_zero`：降级为 0 让调用方跳过，
    /// 而不是让整个保护单计划失败。
    #[test]
    fn quantity_or_zero_degrades_small_amounts_instead_of_failing() {
        let p = prec();
        assert_eq!(p.quantity_or_zero(dec!(0.0001)).unwrap(), Qty::ZERO);
        assert_eq!(p.quantity_or_zero(dec!(1.2349)).unwrap().get(), dec!(1.234));
        // 真正的非法输入仍然报错
        assert!(p.quantity_or_zero(dec!(-1)).is_err());
    }

    #[test]
    fn rejects_non_positive_and_invalid_metadata() {
        let p = prec();
        assert!(
            p.price_for(Side::Buy, dec!(0), PriceRole::PassiveEntry)
                .is_err()
        );
        assert!(
            p.price_for(Side::Buy, dec!(-1), PriceRole::PassiveEntry)
                .is_err()
        );
        assert!(p.quantity(dec!(0)).is_err());
    }

    #[test]
    fn notional_and_min_qty_gate() {
        let p = prec();
        // 0.001 ETH @ 3200 = 3.2 USDT < 5 USDT 最小名义
        let err = p
            .check_order_size(Qty::new(dec!(0.001)), Price::new(dec!(3200)))
            .unwrap_err();
        assert!(matches!(err, DomainError::BelowMinNotional { .. }));
        assert!(
            p.check_order_size(Qty::new(dec!(0.01)), Price::new(dec!(3200)))
                .is_ok()
        );
    }

    /// 量化必须是幂等的：已对齐的值再对齐一次不变。
    /// 这防止策略与风控之间反复量化导致价格漂移。
    #[test]
    fn quantization_is_idempotent() {
        let p = prec();
        for side in [Side::Buy, Side::Sell] {
            for role in [
                PriceRole::PassiveEntry,
                PriceRole::TakeProfit,
                PriceRole::StopLoss,
            ] {
                let once = p.price_for(side, dec!(3200.123456), role).unwrap();
                let twice = p.price_for(side, once.get(), role).unwrap();
                assert_eq!(once, twice, "重复量化不应改变价格");
            }
        }
    }
}
