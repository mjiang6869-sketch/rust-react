//! 区间做市：在近期 K 线的高低点挂单，赚一点就跑，止损放突破位置。
//!
//! # 策略逻辑
//!
//! 1. 取最近 `lookback` 根**已收盘** K 线的高低点作为参考区间。
//! 2. 在区间下沿挂买单、上沿挂卖单（只做一个方向，由 `side_mode` 决定）。
//! 3. 止盈取入场价外 `take_profit_bp` 个基点——小额快跑。
//! 4. 止损放在**突破位置**：买单的止损在区间低点下方 `stop_buffer_bp`，
//!    即"价格跌破区间就认错"。
//!
//! # 为什么这个策略特别依赖成交模型的诚实性
//!
//! 区间高低点是**所有人都看得见**的价位，所以那里必然堆积大量挂单——
//! 我们的单排在队尾。而止盈只有几个 bp，意味着：
//!
//! - 如果回测假设"wick 触价即成交"（M0），我们会在每个触碰区间边界的
//!   K 线上都成交并赚到止盈。
//! - 现实中，wick 触价时我们大概率**没有**成交（排在别人后面），
//!   而真正成交的时候往往是价格**穿过**区间继续走——也就是我们接住了
//!   一个正在突破的走势。
//!
//! 这个不对称正是 M0 会骗人的地方：亏损的交易被完美建模，盈利的交易被
//! 建模成免费。所以本策略是用 M1 检验 M0 是否虚高的最佳试金石。
//!
//! # 只做单边
//!
//! `side_mode` 默认只做买方向。理由是 maker-only 下**止损也是挂单**，
//! 可能不成交；同时挂双边会让两个方向都暴露在这个风险下，且回测时难以
//! 归因。等单边跑通、成交模型被校准后再考虑双边。

use chrono::Duration;
use domain::{
    BreakEvenSpec, EnterRequest, MarketView, ParameterSpec, ProtectionPlan, SizeHint,
    StandDownReason, StopSpec, Strategy, StrategyIntent, TpPlan, TpRung, TrailingSpec, check_entry,
    reference_range, resolve_size,
};
use rust_decimal::Decimal;

/// 做单边还是双边。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SideMode {
    /// 只在区间下沿挂买单（做多）。
    LongOnly,
    /// 只在区间上沿挂卖单（做空）。
    ShortOnly,
}

/// 区间做市策略的参数。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RangeMakerParams {
    /// 参考区间的回看根数（1m K 线）。
    pub lookback: usize,
    /// 止盈距离，单位基点（1 bp = 0.01%）。默认 4 bp。
    #[serde(with = "rust_decimal::serde::str")]
    pub take_profit_bp: Decimal,
    /// 止损放在区间边界外多少个基点。默认 2 bp。
    #[serde(with = "rust_decimal::serde::str")]
    pub stop_buffer_bp: Decimal,
    /// 只做多还是只做空。
    pub side_mode: SideMode,
    /// 仓位占权益的比例。
    #[serde(with = "rust_decimal::serde::str")]
    pub equity_pct: Decimal,
    /// 杠杆。
    #[serde(with = "rust_decimal::serde::str")]
    pub leverage: Decimal,
    /// 挂单有效期（分钟）。到期未成交即撤销。
    pub valid_minutes: i64,
    /// 是否启用保本止损。
    pub break_even: bool,
    /// 是否启用移动止损。
    pub trailing: bool,
    /// 移动止损距离（基点）。
    #[serde(with = "rust_decimal::serde::str")]
    pub trailing_bp: Decimal,
    /// 每根 K 线只允许产生一个信号（避免同一区间反复挂单）。
    pub one_signal_per_bar: bool,
}

impl Default for RangeMakerParams {
    fn default() -> Self {
        Self {
            lookback: 60,
            take_profit_bp: Decimal::new(4, 0),
            stop_buffer_bp: Decimal::new(2, 0),
            side_mode: SideMode::LongOnly,
            equity_pct: Decimal::new(1, 1), // 10%
            leverage: Decimal::from(3),
            valid_minutes: 2,
            break_even: true,
            trailing: false,
            trailing_bp: Decimal::new(10, 0),
            one_signal_per_bar: true,
        }
    }
}

impl RangeMakerParams {
    /// 基点转比例：4 bp -> 0.0004。
    pub fn bp_to_ratio(bp: Decimal) -> Decimal {
        bp / Decimal::from(10_000)
    }

    /// 参数规格：这是本策略参数合法性的**唯一规则来源**。
    ///
    /// engine 与 api 校验参数时都应调用 [`RangeMakerParams::validate`]（它遍历
    /// 这里的规格逐项检查），而不是各自重新写一份范围判断——避免出现"前端
    /// 允许而引擎拒绝"或反过来的分裂。
    pub fn specs() -> Vec<ParameterSpec> {
        vec![
            ParameterSpec {
                key: "lookback".into(),
                label: "参考区间回看根数".into(),
                description:
                    "取最近这么多根已收盘 1m K 线的高低点作为挂单区间。数值越大，区间越宽、\
                     成交越少但每次盈利空间越大。"
                        .into(),
                unit: Some("根".into()),
                default: Decimal::from(60),
                min: Decimal::from(10),
                max: Decimal::from(480),
            },
            ParameterSpec {
                key: "take_profit_bp".into(),
                label: "止盈距离".into(),
                description: "入场价外多少个基点止盈。零手续费下这就是单笔毛利润，所以不能设得太小\
                     （小于一个 tick 会导致量化后无法成交），也不能太大（持仓时间变长会放大\
                     逆向选择的影响）。"
                    .into(),
                unit: Some("基点".into()),
                default: Decimal::new(4, 0),
                min: Decimal::new(1, 0),
                max: Decimal::from(50),
            },
            ParameterSpec {
                key: "stop_buffer_bp".into(),
                label: "止损缓冲".into(),
                description: "止损放在区间边界外多少个基点，即「价格突破区间就认错」。缓冲太小会被\
                     正常波动扫掉，太大则单笔亏损过大。"
                    .into(),
                unit: Some("基点".into()),
                default: Decimal::new(2, 0),
                min: Decimal::new(1, 0),
                max: Decimal::from(50),
            },
            ParameterSpec {
                key: "equity_pct".into(),
                label: "仓位比例".into(),
                description: "每次开仓使用权益的百分比。".into(),
                unit: Some("%".into()),
                default: Decimal::new(1, 1),
                min: Decimal::new(1, 3),
                max: Decimal::ONE,
            },
            ParameterSpec {
                key: "leverage".into(),
                label: "杠杆".into(),
                description: "仓位计算使用的杠杆倍数。注意维持保证金率 2.5%，杠杆越高止损距强平\
                     越近，风控会据此拒绝过近的止损。"
                    .into(),
                unit: Some("倍".into()),
                default: Decimal::from(3),
                min: Decimal::ONE,
                max: Decimal::from(20),
            },
            ParameterSpec {
                key: "valid_minutes".into(),
                label: "挂单有效期".into(),
                description: "挂单超过这么久未成交就撤销，避免长期挂在过时的价位上。".into(),
                unit: Some("分钟".into()),
                default: Decimal::from(2),
                min: Decimal::ONE,
                max: Decimal::from(15),
            },
            ParameterSpec {
                key: "trailing_bp".into(),
                label: "移动止损距离".into(),
                description: "启用移动止损后，止损跟随最优价的距离。".into(),
                unit: Some("基点".into()),
                default: Decimal::from(10),
                min: Decimal::ONE,
                max: Decimal::from(100),
            },
        ]
    }

    /// 按规格的 key 取出对应字段的数值（整数字段转换为 `Decimal`）。
    ///
    /// 只服务于 [`RangeMakerParams::validate`]；不在 [`RangeMakerParams::specs`]
    /// 里的字段（例如 `side_mode`、`break_even` 等非数值/未受限参数）不需要
    /// 出现在这里。
    fn numeric_value(&self, key: &str) -> Option<Decimal> {
        match key {
            "lookback" => Some(Decimal::from(self.lookback)),
            "take_profit_bp" => Some(self.take_profit_bp),
            "stop_buffer_bp" => Some(self.stop_buffer_bp),
            "equity_pct" => Some(self.equity_pct),
            "leverage" => Some(self.leverage),
            "valid_minutes" => Some(Decimal::from(self.valid_minutes)),
            "trailing_bp" => Some(self.trailing_bp),
            _ => None,
        }
    }

    /// 校验参数是否落在 [`RangeMakerParams::specs`] 声明的合法范围内。
    ///
    /// 这是参数合法性检查的唯一入口——engine 与 api 都应调用它，而不是自己
    /// 重新判断某个参数的上下限。
    pub fn validate(&self) -> Result<(), String> {
        fn fmt_dec(d: Decimal) -> String {
            d.normalize().to_string()
        }

        for spec in Self::specs() {
            let value = self.numeric_value(&spec.key).ok_or_else(|| {
                format!("内部错误：参数「{}」缺少数值读取实现，无法校验", spec.label)
            })?;
            if value < spec.min || value > spec.max {
                let is_percent = spec.unit.as_deref() == Some("%");
                let (range_desc, value_desc) = if is_percent {
                    let hundred = Decimal::from(100);
                    (
                        format!(
                            "{}%–{}%",
                            fmt_dec(spec.min * hundred),
                            fmt_dec(spec.max * hundred)
                        ),
                        format!("{}%", fmt_dec(value * hundred)),
                    )
                } else {
                    let unit_suffix = match spec.unit.as_deref() {
                        Some(unit) if !unit.is_empty() => format!(" {unit}"),
                        _ => String::new(),
                    };
                    (
                        format!("{}–{}{}", fmt_dec(spec.min), fmt_dec(spec.max), unit_suffix),
                        fmt_dec(value),
                    )
                };
                return Err(format!(
                    "参数「{}」须在 {}之间，收到 {}",
                    spec.label, range_desc, value_desc
                ));
            }
        }
        Ok(())
    }
}

/// 区间做市策略。
pub struct RangeMaker {
    pub params: RangeMakerParams,
}

impl RangeMaker {
    pub fn new(params: RangeMakerParams) -> Self {
        Self { params }
    }

    pub fn with_defaults() -> Self {
        Self::new(RangeMakerParams::default())
    }
}

impl Strategy for RangeMaker {
    fn id(&self) -> &'static str {
        "range_maker"
    }

    fn name(&self) -> &'static str {
        "区间做市"
    }

    fn warmup_candles(&self) -> usize {
        self.params.lookback
    }

    fn parameters(&self) -> Vec<ParameterSpec> {
        RangeMakerParams::specs()
    }

    fn evaluate(&self, view: &MarketView<'_>) -> Option<StrategyIntent> {
        let p = &self.params;

        // 已有持仓或在途单时不产生新意图。这一条必须在最前面——否则会在
        // 已有仓位时反复挂新单，导致重复暴露。
        if view.has_position || view.has_pending_entry {
            return Some(StrategyIntent::StandDown {
                reason: StandDownReason::AlreadyEngaged,
            });
        }

        // 区间只从已收盘 K 线取。`reference_range` 内部会过滤未收盘的。
        let Some(range) = reference_range(view.candles, p.lookback) else {
            return Some(StrategyIntent::StandDown {
                reason: StandDownReason::InsufficientHistory,
            });
        };

        let tp_ratio = RangeMakerParams::bp_to_ratio(p.take_profit_bp);
        let stop_ratio = RangeMakerParams::bp_to_ratio(p.stop_buffer_bp);

        // 入场价 = 区间边界；止损 = 边界外若干基点（即突破位置）
        let (side, entry, stop) = match p.side_mode {
            SideMode::LongOnly => {
                let entry = range.low;
                let stop = entry * (Decimal::ONE - stop_ratio);
                (domain::Side::Buy, entry, stop)
            }
            SideMode::ShortOnly => {
                let entry = range.high;
                let stop = entry * (Decimal::ONE + stop_ratio);
                (domain::Side::Sell, entry, stop)
            }
        };

        // 止盈价：入场价外若干基点
        let tp_price = match side {
            domain::Side::Buy => entry * (Decimal::ONE + tp_ratio),
            domain::Side::Sell => entry * (Decimal::ONE - tp_ratio),
        };

        // 风控。注意这里用了真实的维持保证金率——旧实现硬编码 0.4% 会让
        // 这个检查在高杠杆下误判并静默拒绝信号。
        let verdict = check_entry(
            view.instrument,
            side,
            entry,
            stop,
            tp_price,
            p.leverage,
            &domain::RiskLimits {
                // 区间做市的止损天然较窄（区间边界外一点点），
                // 但极端行情下区间本身可能很宽，所以上限放宽到 1%。
                max_stop_pct: Decimal::new(1, 2),
                // 盈亏比要求：止盈 4bp / 止损 2bp = 2:1
                min_reward_risk: Decimal::ONE,
                max_feed_staleness_secs: 15,
            },
        );
        if let domain::RiskVerdict::Reject(reason) = verdict {
            return Some(StrategyIntent::StandDown { reason });
        }

        // 仓位规模
        let size = SizeHint::EquityFraction {
            pct: p.equity_pct,
            leverage: p.leverage,
        };
        if resolve_size(
            size,
            domain::Price::new(entry),
            view.equity,
            &view.instrument.precision,
        )
        .is_none()
        {
            return Some(StrategyIntent::StandDown {
                reason: StandDownReason::InsufficientEquity,
            });
        }

        let protection = ProtectionPlan {
            stop: StopSpec::Structural { price: stop },
            break_even: p.break_even.then_some(BreakEvenSpec {
                // 走到 1 倍止损距离的浮盈后，把止损推到入场价
                trigger_r: Decimal::ONE,
                offset: Decimal::ZERO,
            }),
            trailing: p.trailing.then_some(TrailingSpec {
                distance: entry * RangeMakerParams::bp_to_ratio(p.trailing_bp),
                activate_at: None,
            }),
            timed_cancel: Some(view.now + Duration::minutes(p.valid_minutes)),
        };

        Some(StrategyIntent::Enter(Box::new(EnterRequest {
            side,
            entry,
            stop,
            take_profit: TpPlan::Single { pct: tp_ratio },
            protection,
            size,
            valid_until: view.now + Duration::minutes(p.valid_minutes),
        })))
    }
}

/// 分批止盈变体：在区间边界挂单，但止盈分三档。
///
/// 存在的意义是验证分批止盈路径与单档的一致性——两者应当只在上场单的
/// 数量和价格上有区别，其余（止损、保本、风控）完全相同。
pub struct RangeMakerLadder {
    pub params: RangeMakerParams,
    /// 各档的止盈倍数与平仓比例。
    pub rungs: Vec<(Decimal, Decimal)>,
}

impl RangeMakerLadder {
    /// 默认三档：1x/2x/3x 止盈距离，比例 40/30/30。
    pub fn with_defaults() -> Self {
        Self {
            params: RangeMakerParams::default(),
            rungs: vec![
                (Decimal::ONE, Decimal::new(4, 1)),
                (Decimal::from(2), Decimal::new(3, 1)),
                (Decimal::from(3), Decimal::new(3, 1)),
            ],
        }
    }
}

impl Strategy for RangeMakerLadder {
    fn id(&self) -> &'static str {
        "range_maker_ladder"
    }

    fn name(&self) -> &'static str {
        "区间做市（分批止盈）"
    }

    fn warmup_candles(&self) -> usize {
        self.params.lookback
    }

    fn parameters(&self) -> Vec<ParameterSpec> {
        RangeMaker::new(self.params.clone()).parameters()
    }

    fn evaluate(&self, view: &MarketView<'_>) -> Option<StrategyIntent> {
        let p = &self.params;
        if view.has_position || view.has_pending_entry {
            return Some(StrategyIntent::StandDown {
                reason: StandDownReason::AlreadyEngaged,
            });
        }
        let range = reference_range(view.candles, p.lookback)?;
        let tp_ratio = RangeMakerParams::bp_to_ratio(p.take_profit_bp);
        let stop_ratio = RangeMakerParams::bp_to_ratio(p.stop_buffer_bp);

        let (side, entry, stop) = match p.side_mode {
            SideMode::LongOnly => (
                domain::Side::Buy,
                range.low,
                range.low * (Decimal::ONE - stop_ratio),
            ),
            SideMode::ShortOnly => (
                domain::Side::Sell,
                range.high,
                range.high * (Decimal::ONE + stop_ratio),
            ),
        };
        let tp_price = match side {
            domain::Side::Buy => entry * (Decimal::ONE + tp_ratio),
            domain::Side::Sell => entry * (Decimal::ONE - tp_ratio),
        };
        if let domain::RiskVerdict::Reject(reason) = check_entry(
            view.instrument,
            side,
            entry,
            stop,
            tp_price,
            p.leverage,
            &domain::RiskLimits {
                max_stop_pct: Decimal::new(1, 2),
                min_reward_risk: Decimal::ONE,
                max_feed_staleness_secs: 15,
            },
        ) {
            return Some(StrategyIntent::StandDown { reason });
        }

        let rungs: Vec<TpRung> = self
            .rungs
            .iter()
            .map(|(mult, frac)| TpRung {
                pct: tp_ratio * mult,
                fraction: *frac,
            })
            .collect();
        let plan = TpPlan::Ladder { rungs };
        if plan.validate().is_err() {
            return Some(StrategyIntent::StandDown {
                reason: StandDownReason::RiskRewardTooLow,
            });
        }

        Some(StrategyIntent::Enter(Box::new(EnterRequest {
            side,
            entry,
            stop,
            take_profit: plan,
            protection: ProtectionPlan {
                stop: StopSpec::Structural { price: stop },
                break_even: p.break_even.then_some(BreakEvenSpec {
                    trigger_r: Decimal::ONE,
                    offset: Decimal::ZERO,
                }),
                trailing: None,
                timed_cancel: Some(view.now + Duration::minutes(p.valid_minutes)),
            },
            size: SizeHint::EquityFraction {
                pct: p.equity_pct,
                leverage: p.leverage,
            },
            valid_until: view.now + Duration::minutes(p.valid_minutes),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use domain::{Candle, ContractKind, FeeSchedule, FeeSource, Instrument, Precision};
    use rust_decimal_macros::dec;

    fn instrument() -> Instrument {
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

    fn candles(n: usize, low: Decimal, high: Decimal) -> Vec<Candle> {
        let t0: DateTime<Utc> = Utc
            .timestamp_millis_opt(1_785_542_400_000)
            .single()
            .unwrap();
        (0..n)
            .map(|i| Candle {
                open_time: t0 + Duration::minutes(i as i64),
                open: low,
                high,
                low,
                close: high,
                volume: dec!(1),
                closed: true,
            })
            .collect()
    }

    fn view<'a>(candles: &'a [Candle], inst: &'a Instrument, has_position: bool) -> MarketView<'a> {
        MarketView {
            instrument: inst,
            candles,
            now: Utc::now(),
            has_position,
            has_pending_entry: false,
            equity: dec!(10000),
        }
    }

    /// 长期只做多时，入场价应等于区间低点。
    #[test]
    fn long_only_entry_is_at_range_low() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMaker::with_defaults();
        let v = view(&cs, &inst, false);

        match s.evaluate(&v).unwrap() {
            StrategyIntent::Enter(req) => {
                assert_eq!(req.side, domain::Side::Buy);
                assert_eq!(req.entry, dec!(3190), "入场价应为区间低点");
            }
            other => panic!("应产生开仓意图，实际：{other:?}"),
        }
    }

    /// 止损必须放在区间低点**下方**——即"跌破区间就认错"。
    #[test]
    fn stop_is_placed_below_the_range_for_long() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMaker::with_defaults();
        let v = view(&cs, &inst, false);

        match s.evaluate(&v).unwrap() {
            StrategyIntent::Enter(req) => {
                assert!(
                    req.stop < req.entry,
                    "多头止损必须低于入场价：stop={} entry={}",
                    req.stop,
                    req.entry
                );
                // 2 bp 缓冲：3190 * (1 - 0.0002) = 3189.362
                assert_eq!(req.stop, dec!(3189.3620));
            }
            other => panic!("应产生开仓意图，实际：{other:?}"),
        }
    }

    /// 止盈在入场价上方，且距离正确（4 bp）。
    #[test]
    fn take_profit_is_above_entry_by_configured_bp() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMaker::with_defaults();

        // 4 bp = 0.0004；3190 * 1.0004 = 3191.276
        match s.evaluate(&view(&cs, &inst, false)).unwrap() {
            StrategyIntent::Enter(req) => {
                assert_eq!(req.take_profit.rungs()[0].0, dec!(0.0004));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn short_only_entry_is_at_range_high() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMaker::new(RangeMakerParams {
            side_mode: SideMode::ShortOnly,
            ..Default::default()
        });

        match s.evaluate(&view(&cs, &inst, false)).unwrap() {
            StrategyIntent::Enter(req) => {
                assert_eq!(req.side, domain::Side::Sell);
                assert_eq!(req.entry, dec!(3210), "入场价应为区间高点");
                assert!(req.stop > req.entry, "空头止损必须高于入场价");
            }
            other => panic!("{other:?}"),
        }
    }

    /// 已有持仓时必须让位，不能反复挂新单。
    #[test]
    fn stands_down_when_already_engaged() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMaker::with_defaults();

        match s.evaluate(&view(&cs, &inst, true)).unwrap() {
            StrategyIntent::StandDown { reason } => {
                assert_eq!(reason, StandDownReason::AlreadyEngaged);
            }
            other => panic!("{other:?}"),
        }
    }

    /// 历史不足时必须明确让位——静默不交易是恶劣的失败模式。
    #[test]
    fn stands_down_with_insufficient_history() {
        let inst = instrument();
        let cs = candles(5, dec!(3190), dec!(3210));
        let s = RangeMaker::with_defaults();

        match s.evaluate(&view(&cs, &inst, false)).unwrap() {
            StrategyIntent::StandDown { reason } => {
                assert_eq!(reason, StandDownReason::InsufficientHistory);
            }
            other => panic!("历史不足应让位，实际：{other:?}"),
        }
    }

    /// 区间退化（高低同价）时不能挂单。
    #[test]
    fn degenerate_range_stands_down() {
        let inst = instrument();
        let cs = candles(60, dec!(3200), dec!(3200));
        let s = RangeMaker::with_defaults();

        match s.evaluate(&view(&cs, &inst, false)).unwrap() {
            StrategyIntent::StandDown { reason } => {
                assert_eq!(reason, StandDownReason::InsufficientHistory);
            }
            other => panic!("退化区间不应产生信号，实际：{other:?}"),
        }
    }

    /// 权益不足时必须让位而不是下零数量单。
    #[test]
    fn stands_down_with_insufficient_equity() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMaker::with_defaults();
        let mut v = view(&cs, &inst, false);
        v.equity = dec!(1); // 权益极小

        match s.evaluate(&v).unwrap() {
            StrategyIntent::StandDown { reason } => {
                assert_eq!(reason, StandDownReason::InsufficientEquity);
            }
            other => panic!("{other:?}"),
        }
    }

    /// 保护单计划里必须带上定时取消——挂单不能长期留在过时价位。
    #[test]
    fn protection_plan_includes_timed_cancel() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMaker::with_defaults();

        match s.evaluate(&view(&cs, &inst, false)).unwrap() {
            StrategyIntent::Enter(req) => {
                assert!(req.protection.timed_cancel.is_some(), "必须设置挂单有效期");
                assert!(req.protection.break_even.is_some(), "默认应启用保本止损");
                assert!(req.protection.trailing.is_none(), "默认不启用移动止损");
            }
            other => panic!("{other:?}"),
        }
    }

    /// valid_until 必须与 timed_cancel 一致，否则会出现"订单还在挂但策略
    /// 已经认为它过期"的状态分裂。
    #[test]
    fn valid_until_matches_timed_cancel() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMaker::with_defaults();

        match s.evaluate(&view(&cs, &inst, false)).unwrap() {
            StrategyIntent::Enter(req) => {
                assert_eq!(
                    req.valid_until,
                    req.protection.timed_cancel.unwrap(),
                    "有效期与定时取消必须是同一个时刻"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bp_conversion_is_exact() {
        assert_eq!(RangeMakerParams::bp_to_ratio(dec!(4)), dec!(0.0004));
        assert_eq!(RangeMakerParams::bp_to_ratio(dec!(1)), dec!(0.0001));
        assert_eq!(RangeMakerParams::bp_to_ratio(dec!(100)), dec!(0.01));
    }

    /// 分批止盈的各档比例之和必须合法，且档位价格递增。
    #[test]
    fn ladder_variant_produces_increasing_take_profits() {
        let inst = instrument();
        let cs = candles(60, dec!(3190), dec!(3210));
        let s = RangeMakerLadder::with_defaults();

        match s.evaluate(&view(&cs, &inst, false)).unwrap() {
            StrategyIntent::Enter(req) => {
                let rungs = req.take_profit.rungs();
                assert_eq!(rungs.len(), 3, "默认三档");
                assert!(rungs[0].0 < rungs[1].0);
                assert!(rungs[1].0 < rungs[2].0);
                let total: Decimal = rungs.iter().map(|(_, f)| *f).sum();
                assert_eq!(total, Decimal::ONE, "各档比例之和应为 1");
            }
            other => panic!("{other:?}"),
        }
    }

    /// 两个变体的参数必须都能在前端展示解释。
    #[test]
    fn parameters_are_documented_for_ui() {
        let s = RangeMaker::with_defaults();
        let params = s.parameters();
        assert!(!params.is_empty());
        for p in &params {
            assert!(!p.label.is_empty(), "参数 {} 缺少标签", p.key);
            assert!(
                !p.description.is_empty(),
                "参数 {} 缺少说明——前端要靠它解释参数含义",
                p.key
            );
            assert!(
                p.min <= p.default && p.default <= p.max,
                "参数 {} 的默认值超出范围",
                p.key
            );
        }
    }

    /// 把某个规格 key 对应的字段设为给定的 `Decimal` 值。整数字段做类型转换。
    fn set_numeric(p: &mut RangeMakerParams, key: &str, v: Decimal) {
        use rust_decimal::prelude::ToPrimitive;
        match key {
            "lookback" => {
                p.lookback = v
                    .to_usize()
                    .unwrap_or_else(|| panic!("lookback 测试值 {v} 无法转换为 usize"))
            }
            "take_profit_bp" => p.take_profit_bp = v,
            "stop_buffer_bp" => p.stop_buffer_bp = v,
            "equity_pct" => p.equity_pct = v,
            "leverage" => p.leverage = v,
            "valid_minutes" => {
                p.valid_minutes = v
                    .to_i64()
                    .unwrap_or_else(|| panic!("valid_minutes 测试值 {v} 无法转换为 i64"))
            }
            "trailing_bp" => p.trailing_bp = v,
            other => panic!("测试辅助函数 set_numeric 不认识参数 key: {other}"),
        }
    }

    /// 越界扰动步长：整数型字段用 1，其余用一个足以跨越边界的小数步长。
    fn perturb_step(key: &str) -> Decimal {
        match key {
            "lookback" | "valid_minutes" => Decimal::ONE,
            "equity_pct" => dec!(0.0001),
            _ => dec!(0.1),
        }
    }

    /// 默认参数必须通过校验——否则新用户一上来就会被拒。
    #[test]
    fn default_params_pass_validation() {
        assert!(RangeMakerParams::default().validate().is_ok());
    }

    /// 表驱动：每个规格字段在 min/max 边界上应通过，越界一点点应失败，
    /// 且错误文案要点名是哪个参数（用 label）。
    #[test]
    fn boundary_values_validate_per_spec() {
        for spec in RangeMakerParams::specs() {
            let step = perturb_step(&spec.key);

            let mut at_min = RangeMakerParams::default();
            set_numeric(&mut at_min, &spec.key, spec.min);
            assert!(
                at_min.validate().is_ok(),
                "参数 {} 取最小值 {} 应通过校验",
                spec.key,
                spec.min
            );

            let mut at_max = RangeMakerParams::default();
            set_numeric(&mut at_max, &spec.key, spec.max);
            assert!(
                at_max.validate().is_ok(),
                "参数 {} 取最大值 {} 应通过校验",
                spec.key,
                spec.max
            );

            let mut below_min = RangeMakerParams::default();
            set_numeric(&mut below_min, &spec.key, spec.min - step);
            let err = below_min
                .validate()
                .expect_err(&format!("参数 {} 低于最小值应校验失败", spec.key));
            assert!(
                err.contains(&spec.label),
                "错误文案应包含标签「{}」：{err}",
                spec.label
            );

            let mut above_max = RangeMakerParams::default();
            set_numeric(&mut above_max, &spec.key, spec.max + step);
            let err = above_max
                .validate()
                .expect_err(&format!("参数 {} 高于最大值应校验失败", spec.key));
            assert!(
                err.contains(&spec.label),
                "错误文案应包含标签「{}」：{err}",
                spec.label
            );
        }
    }

    /// 百分比参数越界时，错误文案应带 % 号，用比例本身（0.15）会让用户看不懂。
    #[test]
    fn percent_parameter_out_of_range_message_contains_percent_sign() {
        let p = RangeMakerParams {
            equity_pct: dec!(1.5), // 150%，超出上限 100%
            ..Default::default()
        };
        let err = p.validate().expect_err("超出范围应返回错误");
        assert!(err.contains('%'), "百分比参数越界的错误文案应包含 %：{err}");
    }

    /// specs() 里声明的每个 key 都必须能通过 numeric_value 取到值，否则
    /// validate() 会把内部实现缺口误报成参数越界。
    #[test]
    fn numeric_value_resolves_every_spec_key() {
        let p = RangeMakerParams::default();
        for spec in RangeMakerParams::specs() {
            assert!(
                p.numeric_value(&spec.key).is_some(),
                "参数 {} 应能通过 numeric_value 取到数值",
                spec.key
            );
        }
    }

    /// `RangeMaker::parameters()` 与 `RangeMakerParams::specs()` 必须是同一份
    /// key 列表——前者只是后者的委托，不能各自维护一套。
    #[test]
    fn range_maker_parameters_match_specs_keys() {
        let strategy_keys: Vec<String> = RangeMaker::with_defaults()
            .parameters()
            .into_iter()
            .map(|p| p.key)
            .collect();
        let spec_keys: Vec<String> = RangeMakerParams::specs()
            .into_iter()
            .map(|p| p.key)
            .collect();
        assert_eq!(strategy_keys, spec_keys);
    }
}
