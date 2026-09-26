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

// 强平估算**不在这里**：它属于 `crate::margin`。
//
// 这里曾有一对逐仓公式（`liquidation_price_estimate` /
// `stop_precedes_liquidation`），只看杠杆、不看账户余额。本项目统一用全仓，
// 强平价由钱包余额与持仓数量共同决定，所以整套计算搬到了 `domain::margin`，
// 逐仓公式已删除——留着它就会出现两份强平公式，而两份公式必然分叉。

#[cfg(test)]
mod tests {
    use super::*;

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
