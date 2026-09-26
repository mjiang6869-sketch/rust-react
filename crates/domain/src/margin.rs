//! 保证金模式与全仓强平估算。
//!
//! # 统一全仓
//!
//! 本项目**只有全仓（CROSSED）**：回测、模拟盘、实盘走同一套口径。这里
//! 刻意不提供逐仓的计算分支——两种模式并存必然出现两份强平公式，而两份
//! 公式一定会分叉，分叉的强平价会让风控在两个模式下给出相反裁决。
//!
//! 逐仓只以 [`ObservedMarginMode`] 的形式存在：它是「在交易所那边观测到的
//! 状态」，用于实盘对账，不是可配置项，也没有对应的计算逻辑。
//!
//! # 强平估算的来源
//!
//! 按币安 USDⓈ-M 全仓的官方公式，单向持仓、且账户里只有这一个仓位时
//! （其它合约的维持保证金与未实现盈亏都为 0，`cum` 取 0）：
//!
//! ```text
//! 触发条件：WB + s·Q·(P − EP) = Q·P·MMR
//! 解得：    P = (WB − s·Q·EP) / (Q·MMR − s·Q)
//! ```
//!
//! 其中 `s = +1` 为多头、`−1` 为空头，`MMR = maint_margin_pct / 100`。
//! 展开后即 [`cross_liquidation`] 里的两行公式。
//!
//! **单仓近似在本项目里是精确的**：三种引擎都只允许同时存在一个仓位
//! （模拟盘 `place()`、回测、策略都以「已有持仓或在途单则不下单」为前提），
//! 所以开仓时账户里没有别的仓位，钱包余额就是账户权益。
//!
//! ## 刻意忽略的项
//!
//! - **资金费**：会随时间改变钱包余额，开仓时刻无法预知。
//! - **强平手续费**：只影响强平后的结算，不影响触发价。
//! - **维持保证金分档**：只取单档 `maint_margin_pct`。名义价值超出第一档时
//!   真实 MMR 更高，估算出的强平价**偏远**——偏乐观的方向，是已知偏差。
//! - **标记价**：币安按标记价触发强平，这里按传入的入场价线性推导。
//! - **多资产折算**：只计入合约 `margin_asset` 这一个钱包。USDT 抵押品
//!   即使能为本合约提供保证金也不计入——结果是强平价偏近，偏保守，
//!   同时满足「USDT 与 USDC 绝不合并」这条不变量。
//!
//! # 杠杆的语义
//!
//! 全仓下杠杆**只决定两件事**：按比例下单时的数量，以及初始保证金占用
//! （[`initial_margin`]）。它**不进入** [`cross_liquidation`] 的参数——
//! 编译层面就保证杠杆影响不到强平价，这是全仓与逐仓最本质的区别。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::instrument::Instrument;
use crate::money::Qty;
use crate::order::Side;

/// 保证金模式。
///
/// 只有一个取值，因为本项目统一用全仓（见模块文档）。保留成显式类型而
/// 不是省略：DTO、对账与界面文案都需要一个不会被配置成别的值的标识。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarginMode {
    #[default]
    Cross,
}

impl MarginMode {
    /// 币安 `marginType` 的写法。
    pub fn tag(self) -> &'static str {
        match self {
            MarginMode::Cross => "CROSS",
        }
    }

    /// 面向用户的中文名。
    pub fn label(self) -> &'static str {
        match self {
            MarginMode::Cross => "全仓",
        }
    }
}

/// 在交易所侧**观测到**的保证金模式。
///
/// 只用于实盘对账：它对不上全仓时必须阻止武装，而不是切换计算口径。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ObservedMarginMode {
    Cross,
    Isolated,
    #[default]
    Unknown,
}

impl ObservedMarginMode {
    pub fn label(self) -> &'static str {
        match self {
            ObservedMarginMode::Cross => "全仓",
            ObservedMarginMode::Isolated => "逐仓",
            ObservedMarginMode::Unknown => "未知",
        }
    }
}

/// 账户保证金上下文。金额带**显式资产标签**——USDT 与 USDC 绝不折算相加。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarginAccount {
    /// 保证金资产（合约的 `margin_asset`）。
    pub asset: String,
    /// 钱包余额：已结算的现金，不含未实现盈亏。
    #[allow(dead_code)]
    pub wallet_balance: Decimal,
    /// 已占用初始保证金。单仓不变量下开仓前恒为 0。
    pub used_initial_margin: Decimal,
}

#[allow(dead_code)]
impl MarginAccount {
    /// 空仓、无其它占用的账户。
    pub fn flat(asset: impl Into<String>, wallet_balance: Decimal) -> Self {
        Self {
            asset: asset.into(),
            wallet_balance,
            used_initial_margin: Decimal::ZERO,
        }
    }

    /// 可用保证金 = 钱包余额 − 已占用初始保证金。
    ///
    /// 开仓前用它校验初始保证金是否够付。注意**不含**未实现盈亏：这里
    /// 只有开仓前的语义，而单仓不变量下开仓前没有持仓，未实现盈亏为 0。
    pub fn available(&self) -> Decimal {
        self.wallet_balance - self.used_initial_margin
    }
}

/// 待开仓位的暴露：账户 + 方向 + 数量 + 入场价。
#[derive(Clone, Copy, Debug)]
pub struct EntryExposure<'a> {
    pub account: &'a MarginAccount,
    pub side: Side,
    pub entry: Decimal,
    pub quantity: Qty,
    /// 杠杆。只用于初始保证金，不参与强平估算。
    pub leverage: Decimal,
}

/// 初始保证金 = 名义价值 / 杠杆。
///
/// 杠杆按「至少 1 倍」处理：币安不接受小于 1 的杠杆，而这里出现 0 或负数
/// 只会来自错误输入，按 1 倍算比除零崩溃好。
pub fn initial_margin(notional: Decimal, leverage: Decimal) -> Decimal {
    notional / leverage.max(Decimal::ONE)
}

/// 全仓强平估算的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CrossLiquidation {
    /// 任何正价格都不会触发强平（钱包余额足够覆盖名义价值的维持保证金）。
    Never,
    /// 估算的强平触发价。**未做 tick 量化**——它是估算值，不是订单价。
    ///
    /// 价格在 JSON 里是字符串，与全仓库一致，避免浮点丢精度。
    At(#[serde(with = "rust_decimal::serde::str")] Decimal),
    /// 开仓即处于强平线之下（钱包余额连维持保证金都不够）。
    Immediate,
}

/// 无法估算强平的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarginError {
    /// 账户保证金资产与合约保证金资产不一致。绝不折算合并。
    AssetMismatch,
    /// 数量或入场价非正，无法估算。
    NonPositive,
}

impl MarginError {
    pub fn message(self) -> &'static str {
        match self {
            MarginError::AssetMismatch => "账户保证金资产与合约保证金资产不一致，无法估算全仓强平",
            MarginError::NonPositive => "数量或入场价非正，无法估算全仓强平",
        }
    }
}

/// 估算全仓强平价。
///
/// 公式与近似项见模块文档。`Side::Buy` 为多头。
pub fn cross_liquidation(
    instrument: &Instrument,
    account: &MarginAccount,
    side: Side,
    entry: Decimal,
    quantity: Qty,
) -> Result<CrossLiquidation, MarginError> {
    if account.asset != instrument.margin_asset {
        return Err(MarginError::AssetMismatch);
    }
    let q = quantity.get();
    if q <= Decimal::ZERO || entry <= Decimal::ZERO {
        return Err(MarginError::NonPositive);
    }

    let mmr = instrument.maint_margin_pct / Decimal::ONE_HUNDRED;
    let wb = account.wallet_balance;
    let notional = q * entry;

    // 钱包余额连维持保证金都不够：开仓即在强平线之下。
    if wb <= notional * mmr {
        return Ok(CrossLiquidation::Immediate);
    }

    let price = match side {
        // 多头：(Q·EP − WB) / (Q·(1 − MMR))
        Side::Buy => (notional - wb) / (q * (Decimal::ONE - mmr)),
        // 空头：(WB + Q·EP) / (Q·(1 + MMR))
        Side::Sell => (wb + notional) / (q * (Decimal::ONE + mmr)),
    };

    if price <= Decimal::ZERO {
        // 只可能出现在多头：钱包余额已覆盖全部名义价值，价格跌到 0 之前
        // 都不会触及强平线。
        return Ok(CrossLiquidation::Never);
    }
    Ok(CrossLiquidation::At(price))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::{ContractKind, FeeSchedule, FeeSource};
    use crate::precision::Precision;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn instrument(maint_pct: Decimal) -> Instrument {
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
            maint_margin_pct: maint_pct,
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

    /// 只有全仓一种模式，且序列化成币安的写法。
    #[test]
    fn margin_mode_is_cross_only() {
        assert_eq!(MarginMode::default(), MarginMode::Cross);
        assert_eq!(
            serde_json::to_string(&MarginMode::Cross).unwrap(),
            "\"CROSS\""
        );
        assert_eq!(MarginMode::Cross.tag(), "CROSS");
        assert_eq!(MarginMode::Cross.label(), "全仓");
    }

    /// 逐仓只作为观测值存在，不是计算分支。
    #[test]
    fn observed_mode_covers_isolated_as_an_anomaly() {
        assert_eq!(ObservedMarginMode::default(), ObservedMarginMode::Unknown);
        assert_eq!(ObservedMarginMode::Isolated.label(), "逐仓");
    }

    /// 杠杆只影响初始保证金。
    #[test]
    fn initial_margin_divides_by_leverage() {
        assert_eq!(
            initial_margin(dec!(1000), dec!(3)),
            dec!(333.33333333333333333333333333)
        );
        assert_eq!(initial_margin(dec!(1000), dec!(1)), dec!(1000));
        // 小于 1 的杠杆按 1 倍处理，不除零
        assert_eq!(initial_margin(dec!(1000), Decimal::ZERO), dec!(1000));
    }

    /// 表驱动：全仓强平价。每行一种行为。
    ///
    /// 入场 3200、维持保证金率 2.5%（币安真实值），除非该行另注。
    /// 手算依据：多头 `(Q·EP − WB)/(Q·(1−MMR))`，空头 `(WB + Q·EP)/(Q·(1+MMR))`。
    #[test]
    fn cross_liquidation_table() {
        let inst = instrument(dec!(2.5));
        let acct = |wb: Decimal| MarginAccount::flat("USDC", wb);
        let q = |q: Decimal| Qty::new(q);

        struct Row {
            name: &'static str,
            side: Side,
            wb: Decimal,
            qty: Decimal,
            want: CrossLiquidation,
        }
        let rows = [
            Row {
                // (3200−160)/0.975 = 3117.9487…
                name: "多头等效 20 倍",
                side: Side::Buy,
                wb: dec!(160),
                qty: dec!(1),
                want: CrossLiquidation::At(dec!(3117.95)),
            },
            Row {
                // (160+3200)/1.025 = 3278.0487…
                name: "空头等效 20 倍",
                side: Side::Sell,
                wb: dec!(160),
                qty: dec!(1),
                want: CrossLiquidation::At(dec!(3278.05)),
            },
            Row {
                // 默认策略参数：权益 10000、10% 仓位、3 倍杠杆 → 数量 0.937
                // (3200×0.937 − 10000) < 0 → 不会强平
                name: "默认做市参数多头不会强平",
                side: Side::Buy,
                wb: dec!(10000),
                qty: dec!(0.937),
                want: CrossLiquidation::Never,
            },
            Row {
                // (10000 + 2998.4)/(0.937×1.025) = 13534.01 → 远在止损之外
                name: "默认做市参数空头",
                side: Side::Sell,
                wb: dec!(10000),
                qty: dec!(0.937),
                want: CrossLiquidation::At(dec!(13534.01)),
            },
            Row {
                // 恰好 1 倍：WB = Q·EP → 分子为 0
                name: "1 倍杠杆多头边界不会强平",
                side: Side::Buy,
                wb: dec!(3200),
                qty: dec!(1),
                want: CrossLiquidation::Never,
            },
            Row {
                // (3200−81)/0.975 = 3198.97
                name: "权益很小时强平贴近入场价",
                side: Side::Buy,
                wb: dec!(81),
                qty: dec!(1),
                want: CrossLiquidation::At(dec!(3198.97)),
            },
            Row {
                // (81+3200)/1.025 = 3200.98
                name: "空头同理",
                side: Side::Sell,
                wb: dec!(81),
                qty: dec!(1),
                want: CrossLiquidation::At(dec!(3200.98)),
            },
            Row {
                // WB 恰好等于维持保证金 3200×0.025 = 80
                name: "钱包只够维持保证金时开仓即强平",
                side: Side::Buy,
                wb: dec!(80),
                qty: dec!(1),
                want: CrossLiquidation::Immediate,
            },
            Row {
                name: "空头同样",
                side: Side::Sell,
                wb: dec!(50),
                qty: dec!(1),
                want: CrossLiquidation::Immediate,
            },
            Row {
                // 3 倍满仓：Q = 10000×1×3/3200 = 9.375
                // (30000 − 10000)/0.975 = 20512.82 → 多头
                // 手算：(9.375×3200 − 10000)/(9.375×0.975) = 20000/9.140625 = 2188.034…
                name: "3 倍满仓多头",
                side: Side::Buy,
                wb: dec!(10000),
                qty: dec!(9.375),
                want: CrossLiquidation::At(dec!(2188.03)),
            },
            Row {
                // (10000 + 30000)/(9.375×1.025) = 40000/9.609375 = 4162.60…
                name: "3 倍满仓空头",
                side: Side::Sell,
                wb: dec!(10000),
                qty: dec!(9.375),
                want: CrossLiquidation::At(dec!(4162.60)),
            },
            Row {
                name: "零余额立即强平",
                side: Side::Buy,
                wb: Decimal::ZERO,
                qty: dec!(1),
                want: CrossLiquidation::Immediate,
            },
        ];

        for r in rows {
            let got = cross_liquidation(&inst, &acct(r.wb), r.side, dec!(3200), q(r.qty)).unwrap();
            match (got, r.want) {
                (CrossLiquidation::At(a), CrossLiquidation::At(b)) => {
                    assert_eq!(a.round_dp(2), b, "{}：期望强平 {b}，实际 {a}", r.name)
                }
                _ => assert_eq!(got, r.want, "{}", r.name),
            }
        }
    }

    /// 维持保证金率用错会算出更远的强平价——这正是旧实现硬编码 0.4% 的后果。
    #[test]
    fn wrong_maintenance_margin_would_move_liquidation_further_away() {
        let real = instrument(dec!(2.5));
        let wrong = instrument(dec!(0.4));
        let acct = MarginAccount::flat("USDC", dec!(160));
        let at = |i: &Instrument| match cross_liquidation(
            i,
            &acct,
            Side::Buy,
            dec!(3200),
            Qty::new(dec!(1)),
        )
        .unwrap()
        {
            CrossLiquidation::At(p) => p.round_dp(2),
            other => panic!("应能估算：{other:?}"),
        };
        let real_p = at(&real);
        let wrong_p = at(&wrong);
        // 真实 2.5%：(3200−160)/0.975 = 3117.95
        assert_eq!(real_p, dec!(3117.95));
        // 低估到 0.4%：(3200−160)/0.996 = 3052.21 —— 更远，也就是更乐观
        assert_eq!(wrong_p, dec!(3052.21));
        assert!(wrong_p < real_p, "低估维持保证金会算出更远的强平价");
    }

    /// 资产不一致必须报错，绝不折算合并。
    #[test]
    fn asset_mismatch_is_an_error_not_a_conversion() {
        let inst = instrument(dec!(2.5));
        let usdt = MarginAccount::flat("USDT", dec!(10000));
        assert_eq!(
            cross_liquidation(&inst, &usdt, Side::Buy, dec!(3200), Qty::new(dec!(1))),
            Err(MarginError::AssetMismatch)
        );
        assert!(MarginError::AssetMismatch.message().contains("资产"));
    }

    #[test]
    fn non_positive_inputs_are_rejected() {
        let inst = instrument(dec!(2.5));
        let acct = MarginAccount::flat("USDC", dec!(10000));
        assert_eq!(
            cross_liquidation(&inst, &acct, Side::Buy, dec!(3200), Qty::new(Decimal::ZERO)),
            Err(MarginError::NonPositive)
        );
        assert_eq!(
            cross_liquidation(&inst, &acct, Side::Buy, Decimal::ZERO, Qty::new(dec!(1))),
            Err(MarginError::NonPositive)
        );
    }

    /// 全仓下杠杆**不影响**强平价：同一个钱包余额与数量，换杠杆结果不变。
    ///
    /// 这是全仓与逐仓最本质的区别，也是本模块存在的理由。
    #[test]
    fn leverage_cannot_affect_liquidation() {
        let inst = instrument(dec!(2.5));
        let acct = MarginAccount::flat("USDC", dec!(1000));
        // 函数签名里根本没有杠杆——这条测试锁住调用方不会把杠杆乘回来
        let lp = cross_liquidation(&inst, &acct, Side::Buy, dec!(3200), Qty::new(dec!(1))).unwrap();
        assert_eq!(
            lp,
            CrossLiquidation::At(dec!(2256.4102564102564102564102564))
        );
    }

    /// 可用保证金 = 钱包余额 − 已占用。
    #[test]
    fn available_is_wallet_minus_used() {
        let a = MarginAccount {
            asset: "USDC".into(),
            wallet_balance: dec!(1000),
            used_initial_margin: dec!(250),
        };
        assert_eq!(a.available(), dec!(750));
        assert_eq!(MarginAccount::flat("USDC", dec!(10)).available(), dec!(10));
    }
}
