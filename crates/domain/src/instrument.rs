//! 合约定义与费率。
//!
//! 关键点：**维持保证金率必须来自交易所**，不能硬编码。旧实现把
//! `maintMarginPercent` 写死为 0.4%（`Decimal::new(4, 3)`），而币安实际值是
//! 2.5%，相差 6.25 倍。用它判断"止损是否在强平价之前"会让策略在任何像样
//! 的杠杆下误判止损不安全，然后**静默拒绝信号**——策略莫名其妙停止交易，
//! 日志里只有一行风险原因。

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::precision::Precision;

/// 合约类别。决定交易时段语义与默认费率来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContractKind {
    /// 加密永续。USDC 本位合约属于此类（`quoteAsset = USDC`）。
    CryptoPerp,
    /// TradFi 永续（美股/商品），币安 `contractType = TRADIFI_PERPETUAL`。
    /// 结算资产为 USDT。
    TradFiPerp,
}

/// 费率来源。**这不是装饰性字段**——整个策略的 edge 依赖于 maker 零费率
/// 活动，所以必须能区分"交易所账户确认的费率"和"我们假设的费率"。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FeeSource {
    /// 来自 `/fapi/v2/account` 的 `commissionRate`，权威。
    ExchangeAccount,
    /// 来自 `exchangeInfo` 的规则值。
    ExchangeRules,
    /// 零费率活动，**尚未与账户实际费率对账**。
    /// 回测结果必须带 `incomplete` 标记。
    PromotionalAssumed,
    /// 手工配置的兜底值。同样标记回测不完整。
    ConfiguredDefault,
}

impl FeeSource {
    /// 该来源是否可信到能支撑"回测完整"的结论。
    pub const fn is_authoritative(self) -> bool {
        matches!(self, FeeSource::ExchangeAccount | FeeSource::ExchangeRules)
    }
}

/// 费率快照。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeSchedule {
    /// maker 费率，小数表示（0.0002 = 2bp）。
    #[serde(with = "rust_decimal::serde::str")]
    pub maker_rate: Decimal,
    /// taker 费率，小数表示。
    #[serde(with = "rust_decimal::serde::str")]
    pub taker_rate: Decimal,
    pub source: FeeSource,
    pub observed_at: DateTime<Utc>,
}

/// 一个可交易合约的完整规则。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    /// 交易对，例如 `ETHUSDC`、`XAUUSDT`。
    pub symbol: String,
    pub kind: ContractKind,
    pub base_asset: String,
    pub quote_asset: String,
    /// 保证金资产。决定可用余额从哪个钱包扣。
    pub margin_asset: String,
    /// 结算资产。盈亏与手续费记入此处。
    ///
    /// 注意：这与 `margin_asset` 可能不同——币安多资产模式下 USDT 余额
    /// 可以为 USDC 合约提供保证金，但盈亏仍结算在 USDC。两者绝不能相加。
    pub settlement_asset: String,
    pub precision: Precision,

    /// 维持保证金率，来自 `exchangeInfo` 的 `maintMarginPercent`。
    /// 用于判断止损是否会晚于强平触发。
    #[serde(with = "rust_decimal::serde::str")]
    pub maint_margin_pct: Decimal,
    /// 开仓所需保证金率，来自 `requiredMarginPercent`。
    #[serde(with = "rust_decimal::serde::str")]
    pub required_margin_pct: Decimal,
    /// 强平手续费率，来自 `liquidationFee`。
    /// 不是交易手续费，但仓位走到强平就是 taker 级别的成本。
    #[serde(with = "rust_decimal::serde::str")]
    pub liquidation_fee: Decimal,

    pub fees: FeeSchedule,
}

impl Instrument {
    /// 给定入场价与杠杆，估算强平价（简化版，仅用于止损前置校验）。
    ///
    /// 隔离维持保证金后，多头的强平触发价近似为：
    /// `entry * (1 - 1/leverage + maint_margin)`
    ///
    /// 真实强平还涉及账户全仓保证金、其他持仓与资金费，所以这里只做
    /// **保守的下界估计**：它的用途是"止损是否明显晚于强平"，不是精确风控。
    pub fn liquidation_price_estimate(
        &self,
        entry: Decimal,
        leverage: Decimal,
        is_long: bool,
    ) -> Option<Decimal> {
        if leverage <= Decimal::ZERO || entry <= Decimal::ZERO {
            return None;
        }
        let maint = self.maint_margin_pct / Decimal::ONE_HUNDRED;
        let buffer = Decimal::ONE / leverage - maint;
        if buffer <= Decimal::ZERO {
            // 杠杆高到维持保证金已吃掉全部缓冲，任何仓位都会立即强平。
            return None;
        }
        let factor = if is_long {
            Decimal::ONE - buffer
        } else {
            Decimal::ONE + buffer
        };
        let price = entry * factor;
        (price > Decimal::ZERO).then_some(price)
    }

    /// 校验止损价是否先于强平价触发。
    ///
    /// 返回 `Err` 时调用方**必须把原因暴露给用户**，不能像旧实现那样
    /// 静默丢弃信号。
    pub fn stop_precedes_liquidation(
        &self,
        entry: Decimal,
        stop: Decimal,
        leverage: Decimal,
        is_long: bool,
    ) -> Result<(), String> {
        let Some(liq) = self.liquidation_price_estimate(entry, leverage, is_long) else {
            return Err(format!(
                "杠杆 {leverage} 下维持保证金率 {} 已无缓冲空间，无法安全开仓",
                self.maint_margin_pct
            ));
        };
        let ok = if is_long { stop > liq } else { stop < liq };
        if ok {
            Ok(())
        } else {
            Err(format!(
                "止损价 {stop} 不早于估算强平价 {liq}（入场 {entry}，杠杆 {leverage}，\
                 维持保证金率 {}%），该仓位会在止损前被强平",
                self.maint_margin_pct
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precision::Precision;
    use rust_decimal_macros::dec;

    fn instrument(maint: Decimal) -> Instrument {
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
            maint_margin_pct: maint,
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

    /// 币安真实的 maintMarginPercent 是 2.5%。
    #[test]
    fn liquidation_estimate_uses_real_maintenance_margin() {
        let inst = instrument(dec!(2.5));
        // 10 倍杠杆：buffer = 0.1 - 0.025 = 0.075 -> 多头强平约在 92.5%
        let liq = inst
            .liquidation_price_estimate(dec!(3200), dec!(10), true)
            .unwrap();
        assert_eq!(liq, dec!(2960.00));
    }

    /// 旧实现硬编码 0.4% 维持保证金，误算强平价。这个测试用真实的 2.5%
    /// 并断言两者结论不同——即"用错常数会改变风控裁决"。
    #[test]
    fn hardcoded_wrong_margin_would_change_verdict() {
        let entry = dec!(3200);
        let leverage = dec!(20);
        // 20 倍杠杆下真实 buffer = 0.05 - 0.025 = 0.025，强平约在 3120
        let real = instrument(dec!(2.5));
        let liq_real = real
            .liquidation_price_estimate(entry, leverage, true)
            .unwrap();
        assert_eq!(liq_real, dec!(3120.00));

        // 错误的 0.4% 会给出 0.05 - 0.004 = 0.046，强平约在 3052.8——差了 67 点
        let wrong = instrument(dec!(0.4));
        let liq_wrong = wrong
            .liquidation_price_estimate(entry, leverage, true)
            .unwrap();
        assert_ne!(liq_real, liq_wrong);
        assert!(liq_wrong < liq_real, "低估维持保证金会算出更远的强平价");
    }

    #[test]
    fn stop_inside_liquidation_is_rejected_with_reason() {
        let inst = instrument(dec!(2.5));
        // 20 倍杠杆强平约 3120，止损放 3100 就晚于强平了
        let err = inst
            .stop_precedes_liquidation(dec!(3200), dec!(3100), dec!(20), true)
            .unwrap_err();
        assert!(err.contains("强平"), "错误信息必须说清原因：{err}");
    }

    #[test]
    fn stop_outside_liquidation_passes() {
        let inst = instrument(dec!(2.5));
        assert!(
            inst.stop_precedes_liquidation(dec!(3200), dec!(3150), dec!(20), true)
                .is_ok()
        );
    }

    #[test]
    fn only_exchange_sourced_fees_are_authoritative() {
        assert!(FeeSource::ExchangeAccount.is_authoritative());
        assert!(FeeSource::ExchangeRules.is_authoritative());
        assert!(
            !FeeSource::PromotionalAssumed.is_authoritative(),
            "零费率活动未经对账时，回测必须标记为不完整"
        );
        assert!(!FeeSource::ConfiguredDefault.is_authoritative());
    }
}
