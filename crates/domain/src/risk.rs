//! 风控闸门。
//!
//! # 这是唯一允许产生订单意图的确定性检查
//!
//! 与 AI 无关、与策略无关——策略产出意图后必须过这一关。旧实现把这类检查
//! 散落在 `paper.rs` 的 `invalid_reason` / `risk_reason` / `stop_before_liquidation`
//! 三处，且用硬编码的 0.4% 维持保证金率（真实值 2.5%），结果是高杠杆下
//! **静默拒绝信号**：策略莫名停止交易，日志里只有一行原因。
//!
//! 本模块的契约：
//! 1. 所有拒绝都返回**可编程的原因枚举**，不是字符串。
//! 2. 每个原因都有面向用户的中文说明，必须能被展示。
//! 3. 强平距离用 `Instrument` 里来自交易所的真实维持保证金率。

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::instrument::Instrument;
use crate::market::Candle;
use crate::money::Price;
use crate::order::Side;
use crate::strategy::StandDownReason;

/// 风控配置。来自策略参数或手动面板设置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RiskLimits {
    /// 止损距离占入场价的最大比例（例如 0.005 = 0.5%）。
    pub max_stop_pct: Decimal,
    /// 止盈与止损的最小比值。低于此值不开仓。
    ///
    /// # 默认值为什么是 1 而不是 2
    ///
    /// 做市策略天然是 1:1 ~ 2:1 的盈亏比：止盈只有几个 bp（赚一点就跑），
    /// 止损「突破就认错」也只能是几个 bp（再宽就不叫突破了）。而方向性
    /// 交易的直觉是"至少赚两倍"，那个默认值会让绝大多数做市计划被拒——
    /// 而且是静默拒绝，用户只会看到"盈亏比不达标"而不明白为什么。
    ///
    /// maker-only 下**更不该**要求高盈亏比：止损是挂单、可能不成交，
    /// 所以真正的风险不在单笔亏损幅度，而在裸露时长。要求高盈亏比会逼着
    /// 用户把止盈拉远，反而增加持仓时间与逆向选择暴露。
    pub min_reward_risk: Decimal,
    /// 行情新鲜度阈值（秒）。超过则暂停开仓。
    pub max_feed_staleness_secs: i64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_stop_pct: Decimal::new(5, 3), // 0.5%
            // 1:1 而非 2:1。做市的止盈止损都是 bp 级（止盈"赚一点就跑"、
            // 止损"突破就认错"），要求 2:1 会让绝大多数挂单被静默拒绝。
            // 需要更严格时应在策略参数或手动面板里显式配置。
            min_reward_risk: Decimal::ONE,
            max_feed_staleness_secs: 15,
        }
    }
}

/// 风控裁决。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RiskVerdict {
    Pass,
    Reject(StandDownReason),
}

impl RiskVerdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, RiskVerdict::Pass)
    }
}

/// 行情新鲜度检查。
///
/// 回测与实盘共用这一份定义——旧实现把 15 秒阈值写在 `paper.rs` 内部，
/// 而回测侧另有一套判断，两边不一致。
pub fn feed_is_fresh(
    last_event_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    limits: &RiskLimits,
) -> bool {
    match last_event_at {
        None => false,
        Some(t) => now.signed_duration_since(t).num_seconds() <= limits.max_feed_staleness_secs,
    }
}

/// 开仓前的全部确定性检查。
///
/// 检查项与顺序（顺序有实际意义，先报最根本的问题）：
/// 1. 行情新鲜度
/// 2. 价格与止损的合法性（正数、方向正确）
/// 3. 止损距离不超过上限
/// 4. 止损早于估算强平价（用真实维持保证金率）
/// 5. 盈亏比达标
pub fn check_entry(
    instrument: &Instrument,
    side: Side,
    entry: Decimal,
    stop: Decimal,
    take_profit: Decimal,
    leverage: Decimal,
    limits: &RiskLimits,
) -> RiskVerdict {
    if entry <= Decimal::ZERO || stop <= Decimal::ZERO {
        return RiskVerdict::Reject(StandDownReason::StopTooWide);
    }

    // 止损方向必须正确：多头止损在入场价下方，空头在上方。
    // 方向反了会导致"止损"变成立即成交的吃单，或永不被触发。
    let direction_ok = match side {
        Side::Buy => stop < entry,
        Side::Sell => stop > entry,
    };
    if !direction_ok {
        return RiskVerdict::Reject(StandDownReason::StopTooWide);
    }

    // 止损距离上限
    let stop_distance = (entry - stop).abs();
    let stop_pct = stop_distance / entry;
    if stop_pct > limits.max_stop_pct {
        return RiskVerdict::Reject(StandDownReason::StopTooWide);
    }

    // 止损必须早于强平。这里用的是 `Instrument` 里来自 exchangeInfo 的
    // 真实 maintMarginPercent，不是硬编码常数。
    let is_long = side == Side::Buy;
    if instrument
        .stop_precedes_liquidation(entry, stop, leverage, is_long)
        .is_err()
    {
        return RiskVerdict::Reject(StandDownReason::StopInsideLiquidation);
    }

    // 盈亏比
    if take_profit > Decimal::ZERO {
        let reward = (take_profit - entry).abs();
        if stop_distance > Decimal::ZERO {
            let rr = reward / stop_distance;
            if rr < limits.min_reward_risk {
                return RiskVerdict::Reject(StandDownReason::RiskRewardTooLow);
            }
        }
    }

    RiskVerdict::Pass
}

/// 参考区间：过去若干根已收盘 K 线的高低点。
///
/// 这是做市挂单的基础——在近期高低点挂单。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceRange {
    pub low: Decimal,
    pub high: Decimal,
    /// 参与计算的第一根 K 线时间。
    pub from: DateTime<Utc>,
    /// 参与计算的最后一根 K 线时间。
    pub to: DateTime<Utc>,
}

impl PriceRange {
    pub fn width(&self) -> Decimal {
        self.high - self.low
    }

    /// 区间中点。
    pub fn mid(&self) -> Decimal {
        (self.high + self.low) / Decimal::TWO
    }
}

/// 从已收盘 K 线计算参考区间。
///
/// 只接受 `closed == true` 的 K 线——用未收盘 K 线会让回测偷看未来。
/// 返回 `None` 表示数据不足。
pub fn reference_range(candles: &[Candle], lookback: usize) -> Option<PriceRange> {
    let closed: Vec<&Candle> = candles.iter().filter(|c| c.closed).collect();
    if closed.len() < lookback || lookback == 0 {
        return None;
    }
    let window = &closed[closed.len() - lookback..];
    let mut low = window[0].low;
    let mut high = window[0].high;
    for c in window.iter().skip(1) {
        if c.low < low {
            low = c.low;
        }
        if c.high > high {
            high = c.high;
        }
    }
    if high <= low {
        // 区间退化（所有 K 线同价），不是有效区间
        return None;
    }
    Some(PriceRange {
        low,
        high,
        from: window[0].open_time,
        to: window[window.len() - 1].open_time,
    })
}

/// 校验止损价与止盈价与入场价的方向关系，返回量化前的原始价。
///
/// 单独抽出来是因为手动面板也要用——用户在界面上拖动的止损止盈位置必须
/// 经过同一套校验，不能只在策略路径上检查。
pub fn resolve_exit_prices(
    side: Side,
    entry: Decimal,
    stop_pct: Decimal,
    take_profit_pct: Decimal,
) -> (Decimal, Decimal) {
    match side {
        Side::Buy => (
            entry * (Decimal::ONE - stop_pct),
            entry * (Decimal::ONE + take_profit_pct),
        ),
        Side::Sell => (
            entry * (Decimal::ONE + stop_pct),
            entry * (Decimal::ONE - take_profit_pct),
        ),
    }
}

/// 供 UI 展示的强平距离提示。
///
/// maker-only 下这个提示尤其重要：止损是挂单、可能不成交，所以"止损离强平
/// 有多远"直接决定了裸露风险的上限。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiquidationWarning {
    pub liquidation_price: Option<Price>,
    pub stop_price: Price,
    /// 止损到强平价的距离占入场价的比例。越小越危险。
    pub buffer_pct: Option<Decimal>,
    pub dangerous: bool,
}

pub fn liquidation_warning(
    instrument: &Instrument,
    side: Side,
    entry: Decimal,
    stop: Decimal,
    leverage: Decimal,
) -> LiquidationWarning {
    let is_long = side == Side::Buy;
    let liq = instrument.liquidation_price_estimate(entry, leverage, is_long);
    let buffer_pct = liq.map(|l| ((l - stop).abs()) / entry);
    // 止损距强平不足入场价的 0.1% 视为危险
    let dangerous = buffer_pct.is_some_and(|b| b < Decimal::new(1, 3));
    LiquidationWarning {
        liquidation_price: liq.map(Price::new),
        stop_price: Price::new(stop),
        buffer_pct,
        dangerous,
    }
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

    fn candle(open_time_secs: i64, low: Decimal, high: Decimal) -> Candle {
        Candle {
            open_time: chrono::DateTime::from_timestamp(open_time_secs, 0).unwrap(),
            open: low,
            high,
            low,
            close: high,
            volume: dec!(1),
            closed: true,
        }
    }

    /// 默认盈亏比必须是 1:1 而非 2:1。
    ///
    /// 做市的止盈止损都是 bp 级，2:1 会让挂单普遍被静默拒绝。这条测试锁住
    /// 这个默认值，避免以后有人按"方向性交易的直觉"把它改回去。
    #[test]
    fn default_reward_risk_suits_market_making_not_directional_trading() {
        let l = RiskLimits::default();
        assert_eq!(
            l.min_reward_risk,
            Decimal::ONE,
            "做市策略的盈亏比天然接近 1:1，默认要求高于此会让挂单普遍被拒"
        );

        // 典型做市参数：止损 2bp、止盈 4bp —— 盈亏比 2，应当通过
        let i = instr();
        let v = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3199.36), // 2bp
            dec!(3201.28), // 4bp
            dec!(3),
            &l,
        );
        assert!(v.is_pass(), "典型做市参数应通过：{v:?}");

        // 1:1 的做市参数（止损止盈都是 4bp）也应当通过
        let v2 = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3198.72), // 4bp
            dec!(3201.28), // 4bp
            dec!(3),
            &l,
        );
        assert!(v2.is_pass(), "1:1 的做市参数应通过：{v2:?}");
    }

    #[test]
    fn reasonable_entry_passes_all_checks() {
        let i = instr();
        // 入场 3200，止损 3180（0.625%... 超过 0.5% 上限），调整：止损 3190
        let v = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3190), // 0.3125% 距离
            dec!(3240), // 盈亏比 (40/10) = 4
            dec!(10),
            &RiskLimits::default(),
        );
        assert!(v.is_pass(), "合理参数应通过：{v:?}");
    }

    #[test]
    fn stop_on_wrong_side_is_rejected() {
        let i = instr();
        // 多头但止损放在入场价上方 -> 方向错误
        let v = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3210),
            dec!(3240),
            dec!(10),
            &RiskLimits::default(),
        );
        assert_eq!(v, RiskVerdict::Reject(StandDownReason::StopTooWide));
    }

    #[test]
    fn too_wide_stop_is_rejected() {
        let i = instr();
        // 止损距离 1%，超过默认 0.5% 上限
        let v = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3168),
            dec!(3240),
            dec!(10),
            &RiskLimits::default(),
        );
        assert_eq!(v, RiskVerdict::Reject(StandDownReason::StopTooWide));
    }

    /// 这一条对应旧实现最严重的缺陷：用错维持保证金率会改变风控裁决。
    /// 20 倍杠杆强平约 3120，止损 3130 距强平太近必须拒绝。
    #[test]
    fn stop_too_close_to_liquidation_is_rejected_with_correct_margin() {
        let i = instr();
        let v = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3150),
            dec!(3400),
            dec!(20),
            &RiskLimits::default(),
        );
        // 20倍杠杆 buffer = 0.05 - 0.025 = 0.025 -> 强平 3120
        // 止损 3150 在强平之上，距离 30 点 = 0.94% > 0.5% 上限，
        // 所以先被 StopTooWide 拦下。放宽上限后应命中强平检查。
        assert!(!v.is_pass());

        let loose = RiskLimits {
            max_stop_pct: dec!(5),
            ..RiskLimits::default()
        };
        let v2 = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3150),
            dec!(3400),
            dec!(20),
            &loose,
        );
        assert!(v2.is_pass(), "止损 3150 在强平 3120 之前，应通过：{v2:?}");

        // 止损放到 3110（低于强平 3120）必须被拒绝
        let v3 = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3110),
            dec!(3400),
            dec!(20),
            &loose,
        );
        assert_eq!(
            v3,
            RiskVerdict::Reject(StandDownReason::StopInsideLiquidation)
        );
    }

    #[test]
    fn poor_reward_risk_is_rejected() {
        let i = instr();
        // 止损 10 点，止盈 5 点 -> 盈亏比 0.5 < 2
        let v = check_entry(
            &i,
            Side::Buy,
            dec!(3200),
            dec!(3190),
            dec!(3205),
            dec!(10),
            &RiskLimits::default(),
        );
        assert_eq!(v, RiskVerdict::Reject(StandDownReason::RiskRewardTooLow));
    }

    #[test]
    fn short_side_checks_are_mirrored() {
        let i = instr();
        // 空头：止损在入场价上方，止盈在下方
        let v = check_entry(
            &i,
            Side::Sell,
            dec!(3200),
            dec!(3210),
            dec!(3160),
            dec!(10),
            &RiskLimits::default(),
        );
        assert!(v.is_pass(), "空头合理参数应通过：{v:?}");
    }

    #[test]
    fn feed_freshness_uses_shared_threshold() {
        let limits = RiskLimits::default();
        let now = Utc::now();
        assert!(feed_is_fresh(
            Some(now - chrono::Duration::seconds(5)),
            now,
            &limits
        ));
        assert!(!feed_is_fresh(
            Some(now - chrono::Duration::seconds(16)),
            now,
            &limits
        ));
        assert!(
            !feed_is_fresh(None, now, &limits),
            "从未收到行情时必须视为不新鲜"
        );
    }

    #[test]
    fn reference_range_uses_high_low_of_window() {
        let candles: Vec<Candle> = (0..10)
            .map(|i| {
                candle(
                    60 * i,
                    dec!(100) + Decimal::from(i),
                    dec!(110) + Decimal::from(i),
                )
            })
            .collect();
        let r = reference_range(&candles, 5).unwrap();
        // 最后 5 根：i=5..9，low 最小 105，high 最大 119
        assert_eq!(r.low, dec!(105));
        assert_eq!(r.high, dec!(119));
        assert_eq!(r.width(), dec!(14));
    }

    /// 未收盘的 K 线不能参与区间计算——那是偷看未来。
    #[test]
    fn reference_range_ignores_unclosed_candles() {
        let mut candles: Vec<Candle> = (0..5)
            .map(|i| candle(60 * i, dec!(100), dec!(110)))
            .collect();
        // 追加一根未收盘的、极端的 K 线
        candles.push(Candle {
            open_time: chrono::DateTime::from_timestamp(600, 0).unwrap(),
            open: dec!(1),
            high: dec!(9999),
            low: dec!(1),
            close: dec!(1),
            volume: dec!(1),
            closed: false,
        });
        let r = reference_range(&candles, 5).unwrap();
        assert_eq!(r.high, dec!(110), "未收盘 K 线的极值必须被忽略");
    }

    #[test]
    fn reference_range_needs_enough_candles() {
        let candles: Vec<Candle> = (0..3)
            .map(|i| candle(60 * i, dec!(100), dec!(110)))
            .collect();
        assert!(reference_range(&candles, 5).is_none());
    }

    /// 退化区间（高低相同）不能产生信号。
    #[test]
    fn degenerate_range_is_rejected() {
        let candles: Vec<Candle> = (0..5)
            .map(|i| candle(60 * i, dec!(100), dec!(100)))
            .collect();
        assert!(reference_range(&candles, 5).is_none());
    }

    #[test]
    fn exit_prices_mirror_by_side() {
        let (stop, tp) = resolve_exit_prices(Side::Buy, dec!(3200), dec!(0.005), dec!(0.01));
        assert_eq!(stop, dec!(3184));
        assert_eq!(tp, dec!(3232));

        let (stop, tp) = resolve_exit_prices(Side::Sell, dec!(3200), dec!(0.005), dec!(0.01));
        assert_eq!(stop, dec!(3216));
        assert_eq!(tp, dec!(3168));
    }

    /// 止损贴近强平必须被标为危险——这是 maker-only 裸露风险的预警。
    #[test]
    fn liquidation_warning_flags_tight_buffers() {
        let i = instr();
        // 20 倍杠杆强平约 3120。止损 3121 距离极近 -> 危险
        let w = liquidation_warning(&i, Side::Buy, dec!(3200), dec!(3121), dec!(20));
        assert!(w.liquidation_price.is_some());
        assert!(w.dangerous, "止损距强平 1 点应标为危险：{w:?}");

        // 止损 3150 距强平 30 点 = 0.94% -> 不危险
        let w2 = liquidation_warning(&i, Side::Buy, dec!(3200), dec!(3150), dec!(20));
        assert!(!w2.dangerous);
    }
}
