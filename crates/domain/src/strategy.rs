//! 策略接口与意图类型。
//!
//! # 设计要点
//!
//! 策略**只输出意图**（`StrategyIntent`），不构造订单、不计算最终价格。
//! 价格由 `ProtectionPlanner` 统一编译，量化方向由 `Precision` 唯一决定。
//! 这样新增策略时不可能引入"第四份止盈公式"——旧实现正是那样坏掉的。
//!
//! 手动面板复用同一套 `StrategyIntent`，所以"策略下的单"和"手点的单"走的是
//! 完全相同的下单路径。差异只在意图的来源。

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::instrument::Instrument;
use crate::market::{Candle, MarketEvent};
use crate::money::{Price, Qty};
use crate::order::Side;
use crate::protection::{ProtectionPlan, TpPlan};

/// 仓位规模提示。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SizeHint {
    /// 按权益比例与杠杆计算：`floor(equity * pct * leverage / price)`。
    EquityFraction {
        #[serde(with = "rust_decimal::serde::str")]
        pct: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        leverage: Decimal,
    },
    /// 固定数量。
    Fixed(#[serde(with = "rust_decimal::serde::str")] Decimal),
}

/// 开仓意图的完整参数。
///
/// 单独成结构体并装箱，原因有二：
/// 1. `StrategyIntent` 的各个变体大小差异很大，装箱后枚举本身保持小尺寸
///    （每个意图都要在事件循环里传很多次）。
/// 2. 手动面板要构造的正是这一组字段，独立类型让它能直接复用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnterRequest {
    pub side: Side,
    /// 期望的限价（**未量化**；量化由 `Precision::price_for` 完成）。
    pub entry: Decimal,
    /// 结构性止损价（未量化）。
    pub stop: Decimal,
    pub take_profit: TpPlan,
    pub protection: ProtectionPlan,
    pub size: SizeHint,
    /// 意图失效时刻。到期后即使未成交也应撤单。
    pub valid_until: DateTime<Utc>,
}

/// 策略想要做什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StrategyIntent {
    /// 挂一张开仓单。
    Enter(Box<EnterRequest>),
    /// 立即平掉现有仓位。
    ExitNow { reason: ExitReason },
    /// 策略主动让位，本轮不交易。必须带上原因，要展示到界面上。
    ///
    /// 旧实现用 `status: String` 承载这类信息，且在 20 处被赋值，导致"为什么
    /// 没开仓"无法被程序化处理。这里把它做成显式枚举。
    StandDown { reason: StandDownReason },
}

/// 策略让位的原因。全部面向用户可见。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StandDownReason {
    /// 行情陈旧（超过新鲜度阈值）。
    StaleFeed,
    /// 行情未连接。
    FeedDisconnected,
    /// 数据不足（历史 K 线不够算区间）。
    InsufficientHistory,
    /// 当前 K 线尚未收盘。
    CandleNotClosed,
    /// 已有持仓或在途订单。
    AlreadyEngaged,
    /// 该信号已被使用过（去重）。
    SignalAlreadyUsed,
    /// 风控拒绝：止损距离过宽。
    StopTooWide,
    /// 风控拒绝：止损不早于估算强平价。
    StopInsideLiquidation,
    /// 风险收益比不达标。
    RiskRewardTooLow,
    /// 净值不足以开最低仓位。
    InsufficientEquity,
    /// 可用保证金不足以支付该仓位的初始保证金（全仓）。
    InsufficientMargin,
    /// 账户保证金资产与合约保证金资产不一致。
    MarginAssetMismatch,
    /// 交易时段外（TradFi 合约用；加密永续恒为可交易）。
    OutsideTradingHours,
}

impl StandDownReason {
    /// 面向用户的中文说明。
    pub fn message(self) -> &'static str {
        match self {
            StandDownReason::StaleFeed => "行情超过新鲜度阈值，暂停开仓",
            StandDownReason::FeedDisconnected => "行情未连接，暂停开仓",
            StandDownReason::InsufficientHistory => "历史 K 线不足，无法计算参考区间",
            StandDownReason::CandleNotClosed => "当前 K 线尚未收盘，不使用未完成数据",
            StandDownReason::AlreadyEngaged => "已有持仓或在途订单",
            StandDownReason::SignalAlreadyUsed => "该确认时刻的信号已使用过",
            StandDownReason::StopTooWide => "止损距离超过风控上限",
            StandDownReason::StopInsideLiquidation => "止损价晚于估算强平价，会在止损前被强平",
            StandDownReason::RiskRewardTooLow => "止盈止损比不达标",
            StandDownReason::InsufficientEquity => "净值不足以开出满足最小名义价值的仓位",
            StandDownReason::InsufficientMargin => "可用保证金不足以支付该仓位的初始保证金（全仓）",
            StandDownReason::MarginAssetMismatch => {
                "账户保证金资产与合约保证金资产不一致，无法估算全仓强平"
            }
            StandDownReason::OutsideTradingHours => "当前不在该合约的交易时段内",
        }
    }
}

/// 主动平仓的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExitReason {
    /// 信号失效。
    SignalInvalidated,
    /// 区间换桶。
    NewRange,
    /// 达到最大持有时间。
    MaxHoldingTime,
    /// 手动平仓。
    OperatorRequest,
    /// 风控要求减仓。
    RiskReduction,
}

/// 策略可见的市场视图。
///
/// 刻意是**只读切片**而非完整引擎状态：策略不该知道订单状态机的细节，
/// 那些由 `OrderRouter` 管。这也让策略可以被无依赖地单元测试。
pub struct MarketView<'a> {
    pub instrument: &'a Instrument,
    /// 已收盘的 K 线，按时间升序。
    pub candles: &'a [Candle],
    pub now: DateTime<Utc>,
    /// 当前是否有持仓。
    pub has_position: bool,
    /// 当前是否有在途开仓单。
    pub has_pending_entry: bool,
    /// 可用权益（用于计算仓位规模）。
    pub equity: Decimal,
}

/// 策略插件接口。
///
/// 实现者只需关心"看到这些行情和状态，我想做什么"。所有下单、量化、
/// 保护单推导由框架完成，所以策略不可能绕过风控或引入价格计算分叉。
pub trait Strategy: Send + Sync {
    /// 策略标识。用于订单 ID 前缀与结果记录。
    ///
    /// 注意：**不参与订单 ID 的构造**（旧实现把 `"retest"` 硬编码进
    /// client order id，导致不同策略在同一秒撞单）。这里只用于展示与统计。
    fn id(&self) -> &'static str;

    /// 面向用户的名称。
    fn name(&self) -> &'static str;

    /// 策略参数的人类可读说明，前端用于解释每个参数。
    fn parameters(&self) -> Vec<ParameterSpec>;

    /// 核心决策：看行情与状态，产出意图。
    ///
    /// 返回 `None` 表示"本轮不表态"（既不开仓也不平仓），等价于什么都不做。
    /// 需要表白"我为什么不交易"时用 `StrategyIntent::StandDown`。
    fn evaluate(&self, view: &MarketView<'_>) -> Option<StrategyIntent>;

    /// 该策略需要多少根已收盘 K 线才能决策。不足则 `InsufficientHistory`。
    fn warmup_candles(&self) -> usize;

    /// 处理单个行情事件的默认实现。
    ///
    /// 大多数策略只看 K 线，所以默认只对 K 线反应。需要逐笔信息的策略
    /// （例如按成交量判断突破有效性）可以覆盖它。
    fn on_event(&self, event: &MarketEvent, _view: &MarketView<'_>) -> bool {
        matches!(event, MarketEvent::Kline(c) if c.closed)
    }
}

/// 策略参数说明。前端用它渲染表单并解释每个参数的含义。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterSpec {
    pub key: String,
    pub label: String,
    pub description: String,
    /// 单位说明（例如 "bp"、"% "、"分钟"）。为 `None` 表示无量纲。
    pub unit: Option<String>,
    pub default: Decimal,
    pub min: Decimal,
    pub max: Decimal,
}

/// 把 `SizeHint` 解析为具体数量，并按精度量化。
///
/// 返回 `None` 表示量化后数量为零（权益不足或低于交易所 step），
/// 调用方应据此产出 `StandDownReason::InsufficientEquity`。
pub fn resolve_size(
    hint: SizeHint,
    price: Price,
    equity: Decimal,
    precision: &crate::precision::Precision,
) -> Option<Qty> {
    let raw = match hint {
        SizeHint::EquityFraction { pct, leverage } => {
            let notional = equity * pct * leverage;
            if price.get() <= Decimal::ZERO {
                return None;
            }
            notional / price.get()
        }
        SizeHint::Fixed(q) => q,
    };
    let qty = precision.quantity_or_zero(raw).ok()?;
    if qty.is_zero() { None } else { Some(qty) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precision::Precision;
    use rust_decimal_macros::dec;

    fn prec() -> Precision {
        Precision {
            tick_size: dec!(0.01),
            step_size: dec!(0.001),
            min_qty: dec!(0.001),
            min_notional: dec!(5),
        }
    }

    /// 按权益比例计算仓位：10000 USDC × 10% × 3 倍 = 3000 名义，
    /// 价格 3200 时约为 0.9375，按 step 向下取整为 0.937。
    #[test]
    fn equity_fraction_size_is_quantized_down() {
        let qty = resolve_size(
            SizeHint::EquityFraction {
                pct: dec!(0.1),
                leverage: dec!(3),
            },
            Price::new(dec!(3200)),
            dec!(10000),
            &prec(),
        )
        .unwrap();
        assert_eq!(qty.get(), dec!(0.937));
    }

    /// 权益不足导致量化后为零时必须返回 `None`，让调用方能产出可见原因。
    #[test]
    fn insufficient_equity_yields_none_not_a_zero_order() {
        let got = resolve_size(
            SizeHint::EquityFraction {
                pct: dec!(0.0001),
                leverage: dec!(1),
            },
            Price::new(dec!(3200)),
            dec!(1),
            &prec(),
        );
        assert!(got.is_none(), "量化后为零不能下单，应返回 None");
    }

    #[test]
    fn fixed_size_is_respected() {
        let qty = resolve_size(
            SizeHint::Fixed(dec!(1.2345)),
            Price::new(dec!(100)),
            dec!(0),
            &prec(),
        )
        .unwrap();
        assert_eq!(qty.get(), dec!(1.234));
    }

    /// 零价格必须返回 None 而不是除零崩溃。
    #[test]
    fn zero_price_does_not_panic() {
        let got = resolve_size(
            SizeHint::EquityFraction {
                pct: dec!(0.1),
                leverage: dec!(1),
            },
            Price::new(dec!(0)),
            dec!(10000),
            &prec(),
        );
        assert!(got.is_none());
    }

    /// 每个让位原因都必须有面向用户的中文说明——静默不交易是恶劣的失败模式。
    #[test]
    fn every_stand_down_reason_has_a_message() {
        for r in [
            StandDownReason::StaleFeed,
            StandDownReason::FeedDisconnected,
            StandDownReason::InsufficientHistory,
            StandDownReason::CandleNotClosed,
            StandDownReason::AlreadyEngaged,
            StandDownReason::SignalAlreadyUsed,
            StandDownReason::StopTooWide,
            StandDownReason::StopInsideLiquidation,
            StandDownReason::RiskRewardTooLow,
            StandDownReason::InsufficientEquity,
            StandDownReason::InsufficientMargin,
            StandDownReason::MarginAssetMismatch,
            StandDownReason::OutsideTradingHours,
        ] {
            assert!(!r.message().is_empty(), "{r:?} 缺少说明");
        }
    }
}
