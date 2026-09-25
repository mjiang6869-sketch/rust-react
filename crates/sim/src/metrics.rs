//! 回测指标。
//!
//! # 为什么这些指标比 P&L 重要
//!
//! 零手续费做市的 P&L 是**成交假设的函数**。同一份策略在同一份数据上，
//! 乐观模型和诚实模型可以给出符号相反的结论。所以回测结果必须自带
//! "这个结论有多可信"的元信息，否则看了也没用。
//!
//! 四个反自欺指标：
//!
//! 1. **`breakeven_fill_rate`** —— M1 需要达到 M0 假设成交量的百分之多少，
//!    策略才不亏。大于 0.8 说明这是**成交率赌注**而非做市 edge。
//! 2. **markout** —— 每笔成交后 +1s/+5s/+30s/+5m 的价格变动。
//!    **这个指标比 P&L 更重要**：如果成交后价格系统性地朝不利方向走，
//!    说明你在被逆向选择，那么零手续费也救不了你。
//! 3. **止损裸露分析** —— maker-only 特有。止损是挂单、可能不成交，
//!    跳空穿过它时仓位持续裸露。必须量化有多少笔、裸露多久、如何了结。
//! 4. **费率来源标记** —— 整个 edge 依赖零费率活动。费率为"活动假设"时
//!    结果必须标记为不完整。

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use domain::FeeSource;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 一次成交的 markout 观测。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkoutObservation {
    /// 成交时刻。
    pub at: DateTime<Utc>,
    /// 成交价。
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    /// 成交方向（我们的持仓方向）。
    pub side: MarkoutSide,
    /// 各时间窗口后的价格变动，**已按对我们有利的方向取正负**。
    ///
    /// 正数 = 价格朝我们有利方向走（我们赚了），负数 = 逆向选择。
    #[serde(with = "rust_decimal::serde::str")]
    pub markout_1s: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub markout_5s: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub markout_30s: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub markout_5m: Decimal,
}

/// markout 的方向标记。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarkoutSide {
    Long,
    Short,
}

/// markout 的统计汇总。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkoutSummary {
    pub samples: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub mean_1s: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub mean_5s: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub mean_30s: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub mean_5m: Decimal,
    /// 逆向选择比例：markout 为负（价格朝不利方向走）的成交占比。
    #[serde(with = "rust_decimal::serde::str")]
    pub adverse_ratio_5s: Decimal,
}

impl MarkoutSummary {
    /// 是否存在系统性逆向选择。
    ///
    /// 判定用 5 秒口径：成交后 5 秒内价格平均朝不利方向走，说明我们接到的
    /// 都是"聪明钱"。做市策略在这种情况下长期必然亏损，与手续费无关。
    pub fn has_systematic_adverse_selection(&self) -> bool {
        self.samples > 0 && self.mean_5s < Decimal::ZERO
    }
}

pub fn summarize_markouts(obs: &[MarkoutObservation]) -> MarkoutSummary {
    if obs.is_empty() {
        return MarkoutSummary {
            samples: 0,
            mean_1s: Decimal::ZERO,
            mean_5s: Decimal::ZERO,
            mean_30s: Decimal::ZERO,
            mean_5m: Decimal::ZERO,
            adverse_ratio_5s: Decimal::ZERO,
        };
    }
    let n = Decimal::from(obs.len());
    let sum = |f: fn(&MarkoutObservation) -> Decimal| -> Decimal {
        obs.iter().map(f).sum::<Decimal>() / n
    };
    let adverse = obs.iter().filter(|o| o.markout_5s < Decimal::ZERO).count();
    MarkoutSummary {
        samples: obs.len(),
        mean_1s: sum(|o| o.markout_1s),
        mean_5s: sum(|o| o.markout_5s),
        mean_30s: sum(|o| o.markout_30s),
        mean_5m: sum(|o| o.markout_5m),
        adverse_ratio_5s: Decimal::from(adverse) / n,
    }
}

/// 止损裸露观测：止损被触发但未成交的一段时间。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopExposure {
    /// 止损应被触发的时刻。
    pub triggered_at: DateTime<Utc>,
    /// 最终了结的时刻。
    pub resolved_at: DateTime<Utc>,
    /// 了结方式。
    pub resolution: StopResolution,
    /// 了结价格。
    #[serde(with = "rust_decimal::serde::str")]
    pub resolved_price: Decimal,
    /// 止损挂单价（理想了结价）。
    #[serde(with = "rust_decimal::serde::str")]
    pub stop_price: Decimal,
    /// 相对止损价的滑点（正数 = 比理想价格差）。
    #[serde(with = "rust_decimal::serde::str")]
    pub slippage: Decimal,
}

/// 裸露仓位最终怎么了结。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StopResolution {
    /// 价格回到止损价、限价单成交。
    StopFilled,
    /// 止损价被跳空穿过，但后续有反向成交接走了（价格回来了）。
    FilledLater,
    /// 走到强平。
    Liquidated,
    /// 回测结束时仍未了结。
    StillOpenAtEnd,
}

/// 止损裸露统计。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopExposureSummary {
    /// 止损被触发但没有立即成交的次数。
    pub events: usize,
    /// 其中最终以止损价附近成交的比例。
    #[serde(with = "rust_decimal::serde::str")]
    pub filled_ratio: Decimal,
    /// 走到强平的比例。
    #[serde(with = "rust_decimal::serde::str")]
    pub liquidated_ratio: Decimal,
    /// 裸露时长的中位数（秒）。
    pub median_exposure_secs: i64,
    /// 裸露时长的最大值（秒）。
    pub max_exposure_secs: i64,
    /// 相对理想止损价的平均滑点。
    #[serde(with = "rust_decimal::serde::str")]
    pub mean_slippage: Decimal,
}

pub fn summarize_stop_exposures(events: &[StopExposure]) -> StopExposureSummary {
    if events.is_empty() {
        return StopExposureSummary {
            events: 0,
            filled_ratio: Decimal::ZERO,
            liquidated_ratio: Decimal::ZERO,
            median_exposure_secs: 0,
            max_exposure_secs: 0,
            mean_slippage: Decimal::ZERO,
        };
    }

    let n = events.len();
    let n_dec = Decimal::from(n);
    let filled = events
        .iter()
        .filter(|e| {
            matches!(
                e.resolution,
                StopResolution::StopFilled | StopResolution::FilledLater
            )
        })
        .count();
    let liquidated = events
        .iter()
        .filter(|e| e.resolution == StopResolution::Liquidated)
        .count();

    let mut durations: Vec<i64> = events
        .iter()
        .map(|e| (e.resolved_at - e.triggered_at).num_seconds().max(0))
        .collect();
    durations.sort_unstable();
    let median = durations[durations.len() / 2];
    let max = *durations.last().unwrap_or(&0);
    let mean_slippage: Decimal = events.iter().map(|e| e.slippage).sum::<Decimal>() / n_dec;

    StopExposureSummary {
        events: n,
        filled_ratio: Decimal::from(filled) / n_dec,
        liquidated_ratio: Decimal::from(liquidated) / n_dec,
        median_exposure_secs: median,
        max_exposure_secs: max,
        mean_slippage,
    }
}

/// 延迟拒单统计。
///
/// post-only 单在价格已穿过我们价位时会被交易所拒绝（币安错误码 5022），
/// 且这类订单**不记入订单历史**、**不推送事件**。做市全靠 GTX 挂单，所以
/// 这是常态而非异常，必须显式统计。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyStats {
    /// 尝试提交的总数。
    pub attempts: usize,
    /// 因价格已动而被拒的数量。
    pub rejected_post_only: usize,
    /// 提交延迟假设（毫秒）。
    pub assumed_latency_ms: u64,
}

impl LatencyStats {
    pub fn rejection_ratio(&self) -> Decimal {
        if self.attempts == 0 {
            return Decimal::ZERO;
        }
        Decimal::from(self.rejected_post_only) / Decimal::from(self.attempts)
    }
}

/// 反自欺指标集合。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeMetrics {
    /// M0（乐观上界）的最终权益。
    #[serde(with = "rust_decimal::serde::str")]
    pub m0_final_equity: Decimal,
    /// M1（诚实下界）的最终权益。
    #[serde(with = "rust_decimal::serde::str")]
    pub m1_final_equity: Decimal,
    /// **盈亏平衡成交率**：M1 需要达到 M0 假设成交量的百分之多少才不亏。
    ///
    /// - `Some(0.0)` 附近：成交假设几乎不影响结论，策略稳健
    /// - `Some(0.8)` 以上：这是**成交率赌注**，不是做市 edge
    /// - `Some(1.0)` 以上：即使在乐观模型下也不赚，策略本身有问题
    /// - `None`：M0 本身不盈利，无法定义该比率
    pub breakeven_fill_rate: Option<Decimal>,
    /// 符号是否一致。不一致时**必须**在界面上警告。
    pub sign_flips: bool,
    pub markout: MarkoutSummary,
    pub stop_exposure: StopExposureSummary,
    pub latency: LatencyStats,
}

impl EdgeMetrics {
    /// 结论是否可信。
    pub fn is_conclusive(&self) -> bool {
        !self.sign_flips
            && !self.markout.has_systematic_adverse_selection()
            && self
                .breakeven_fill_rate
                .is_some_and(|r| r < Decimal::new(8, 1))
    }

    /// 面向用户的一句话结论。
    pub fn verdict(&self) -> &'static str {
        if self.sign_flips {
            "结论不可信：乐观模型与诚实模型给出相反的盈亏方向。策略依赖不现实的成交假设。"
        } else if self.markout.has_systematic_adverse_selection() {
            "存在系统性逆向选择：成交后价格平均朝不利方向走。即使零手续费也难以盈利。"
        } else if self
            .breakeven_fill_rate
            .is_some_and(|r| r >= Decimal::new(8, 1))
        {
            "这是成交率赌注而非做市 edge：需要实现乐观模型 80% 以上的成交量才能不亏。"
        } else if self.breakeven_fill_rate.is_none() {
            "乐观模型下即不盈利，策略本身没有 edge。"
        } else {
            "结论相对稳健：诚实模型下仍盈利，且无系统性逆向选择。"
        }
    }
}

/// 费率诚实标记。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeHonesty {
    pub source: FeeSource,
    #[serde(with = "rust_decimal::serde::str")]
    pub maker_rate_used: Decimal,
    /// 若按常规费率（0.02%）计算，结果会差多少。
    ///
    /// 整个 edge 依赖零费率活动。这个字段让"多少收益来自活动"一目了然。
    #[serde(with = "rust_decimal::serde::str")]
    pub fee_contribution: Decimal,
    /// 结果是否不完整（费率非权威来源）。
    pub incomplete: bool,
}

/// 数据缺口摘要。跨越缺口的回测会凭空发明成交，必须阻断。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapSummary {
    pub gap_count: usize,
    pub total_missing_secs: i64,
    /// 允许跨越缺口时标记，结果不可用于决策。
    pub allowed_by_flag: bool,
}

/// 一次回测的完整结果元信息。**这些字段不是装饰，是结论可信度的组成部分。**
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestProvenance {
    pub symbol: String,
    pub strategy_id: String,
    /// 策略参数快照。回测必须可复现。
    pub strategy_params: BTreeMap<String, String>,
    pub fill_model: String,
    pub fill_model_optimism: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub candle_count: usize,
    pub trade_count: usize,
    pub fees: FeeHonesty,
    pub gaps: GapSummary,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn obs(m1: Decimal, m5: Decimal) -> MarkoutObservation {
        MarkoutObservation {
            at: Utc::now(),
            price: dec!(3200),
            side: MarkoutSide::Long,
            markout_1s: m1,
            markout_5s: m5,
            markout_30s: m5,
            markout_5m: m5,
        }
    }

    #[test]
    fn empty_markouts_yield_zero_summary() {
        let s = summarize_markouts(&[]);
        assert_eq!(s.samples, 0);
        assert!(!s.has_systematic_adverse_selection());
    }

    /// 成交后平均朝不利方向走 = 系统性逆向选择。这是做市的致命信号。
    #[test]
    fn negative_markouts_indicate_adverse_selection() {
        let obs = vec![obs(dec!(-1), dec!(-2)), obs(dec!(-1.5), dec!(-3))];
        let s = summarize_markouts(&obs);
        assert!(s.has_systematic_adverse_selection(), "{s:?}");
        assert_eq!(s.adverse_ratio_5s, dec!(1));
    }

    #[test]
    fn positive_markouts_mean_no_adverse_selection() {
        let obs = vec![obs(dec!(1), dec!(2)), obs(dec!(0.5), dec!(1))];
        let s = summarize_markouts(&obs);
        assert!(!s.has_systematic_adverse_selection());
        assert_eq!(s.adverse_ratio_5s, dec!(0));
    }

    #[test]
    fn adverse_ratio_counts_only_negative_5s() {
        let obs = vec![
            obs(dec!(1), dec!(-1)),
            obs(dec!(1), dec!(1)),
            obs(dec!(1), dec!(-0.5)),
            obs(dec!(1), dec!(2)),
        ];
        let s = summarize_markouts(&obs);
        assert_eq!(s.adverse_ratio_5s, dec!(0.5), "4 笔中 2 笔为负");
    }

    fn exposure(res: StopResolution, secs: i64, slip: Decimal) -> StopExposure {
        let t0 = Utc::now();
        StopExposure {
            triggered_at: t0,
            resolved_at: t0 + chrono::Duration::seconds(secs),
            resolution: res,
            resolved_price: dec!(3190),
            stop_price: dec!(3190),
            slippage: slip,
        }
    }

    #[test]
    fn empty_exposures_yield_zero_summary() {
        let s = summarize_stop_exposures(&[]);
        assert_eq!(s.events, 0);
        assert_eq!(s.median_exposure_secs, 0);
    }

    /// 一半止损成交、一半走到强平，比例必须准确反映。
    #[test]
    fn stop_exposure_ratios_are_computed() {
        let events = vec![
            exposure(StopResolution::StopFilled, 1, dec!(0)),
            exposure(StopResolution::FilledLater, 30, dec!(5)),
            exposure(StopResolution::Liquidated, 120, dec!(50)),
            exposure(StopResolution::Liquidated, 60, dec!(40)),
        ];
        let s = summarize_stop_exposures(&events);
        assert_eq!(s.events, 4);
        assert_eq!(s.filled_ratio, dec!(0.5), "2/4 成交");
        assert_eq!(s.liquidated_ratio, dec!(0.5), "2/4 强平");
        assert_eq!(s.mean_slippage, dec!(23.75));
    }

    /// 裸露时长统计要能揭示"仓位裸露了很久"这种风险。
    #[test]
    fn exposure_duration_stats_reveal_long_exposures() {
        let events = vec![
            exposure(StopResolution::FilledLater, 1, dec!(0)),
            exposure(StopResolution::FilledLater, 2, dec!(0)),
            exposure(StopResolution::Liquidated, 3600, dec!(100)),
        ];
        let s = summarize_stop_exposures(&events);
        assert_eq!(s.max_exposure_secs, 3600, "暴露了 1 小时");
        assert_eq!(s.median_exposure_secs, 2);
    }

    /// 符号翻转是最重要的警报：乐观模型赚、诚实模型亏。
    #[test]
    fn sign_flip_is_flagged_and_blocks_conclusive_verdict() {
        let m = EdgeMetrics {
            m0_final_equity: dec!(12000),
            m1_final_equity: dec!(9500),
            breakeven_fill_rate: Some(dec!(1.4)),
            sign_flips: true,
            markout: summarize_markouts(&[]),
            stop_exposure: summarize_stop_exposures(&[]),
            latency: LatencyStats::default(),
        };
        assert!(!m.is_conclusive(), "符号翻转时结论不可信");
        assert!(m.verdict().contains("不可信"));
    }

    #[test]
    fn high_breakeven_fill_rate_indicates_a_fill_rate_bet() {
        let m = EdgeMetrics {
            m0_final_equity: dec!(12000),
            m1_final_equity: dec!(10100),
            breakeven_fill_rate: Some(dec!(0.92)),
            sign_flips: false,
            markout: summarize_markouts(&[obs(dec!(1), dec!(1))]),
            stop_exposure: summarize_stop_exposures(&[]),
            latency: LatencyStats::default(),
        };
        assert!(!m.is_conclusive());
        assert!(m.verdict().contains("成交率赌注"), "{}", m.verdict());
    }

    /// 乐观模型本身不盈利时无法定义盈亏平衡成交率。
    #[test]
    fn no_breakeven_rate_when_upper_bound_is_unprofitable() {
        let m = EdgeMetrics {
            m0_final_equity: dec!(9800),
            m1_final_equity: dec!(9500),
            breakeven_fill_rate: None,
            sign_flips: false,
            markout: summarize_markouts(&[]),
            stop_exposure: summarize_stop_exposures(&[]),
            latency: LatencyStats::default(),
        };
        assert!(!m.is_conclusive());
        assert!(m.verdict().contains("没有 edge"), "{}", m.verdict());
    }

    /// 逆向选择单独就能否决结论，即使盈亏为正。
    #[test]
    fn adverse_selection_alone_blocks_conclusive_verdict() {
        let m = EdgeMetrics {
            m0_final_equity: dec!(12000),
            m1_final_equity: dec!(11000),
            breakeven_fill_rate: Some(dec!(0.4)),
            sign_flips: false,
            markout: summarize_markouts(&[obs(dec!(-1), dec!(-2))]),
            stop_exposure: summarize_stop_exposures(&[]),
            latency: LatencyStats::default(),
        };
        assert!(!m.is_conclusive());
        assert!(m.verdict().contains("逆向选择"), "{}", m.verdict());
    }

    #[test]
    fn healthy_result_is_conclusive() {
        let m = EdgeMetrics {
            m0_final_equity: dec!(12000),
            m1_final_equity: dec!(11200),
            breakeven_fill_rate: Some(dec!(0.45)),
            sign_flips: false,
            markout: summarize_markouts(&[obs(dec!(2), dec!(3))]),
            stop_exposure: summarize_stop_exposures(&[]),
            latency: LatencyStats::default(),
        };
        assert!(m.is_conclusive(), "{}", m.verdict());
        assert!(m.verdict().contains("稳健"), "{}", m.verdict());
    }

    #[test]
    fn latency_rejection_ratio_handles_zero_attempts() {
        let s = LatencyStats::default();
        assert_eq!(s.rejection_ratio(), Decimal::ZERO, "不能除零");
    }

    /// post-only 被拒是常态——做市全靠 GTX 挂单。比例必须算得出来。
    #[test]
    fn latency_rejection_ratio_is_computed() {
        let s = LatencyStats {
            attempts: 200,
            rejected_post_only: 50,
            assumed_latency_ms: 100,
        };
        assert_eq!(s.rejection_ratio(), dec!(0.25));
    }

    /// 费率来源非权威时结果必须标记不完整——整个 edge 依赖零费率活动。
    #[test]
    fn non_authoritative_fee_marks_result_incomplete() {
        let honest = FeeHonesty {
            source: FeeSource::ExchangeAccount,
            maker_rate_used: Decimal::ZERO,
            fee_contribution: dec!(0),
            incomplete: false,
        };
        assert!(!honest.incomplete);

        let assumed = FeeHonesty {
            source: FeeSource::PromotionalAssumed,
            maker_rate_used: Decimal::ZERO,
            fee_contribution: dec!(120),
            incomplete: true,
        };
        assert!(assumed.incomplete, "活动费率未经对账时必须标记不完整");
    }
}
