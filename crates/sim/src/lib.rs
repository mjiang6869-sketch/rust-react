//! 撮合与回测。
//!
//! # 本 crate 的地位
//!
//! 唯一会影响业务结论的部分。零手续费做市下，单笔毛利润就是止盈距离
//! （约 4bp），所以**成交模型的保真度直接决定策略是否"看起来"赚钱**。
//! 一个乐观 10% 的成交假设可以让任何策略盈利。
//!
//! # 与 domain 的关系
//!
//! `domain` 定义订单生命周期与保护单数学，本 crate 提供"这笔挂单到底成没成"
//! 的判定。回测与模拟盘用**同一个** `SimAdapter`，只是数据来源不同：
//! 回测喂 Parquet，模拟盘喂实时 socket。所以"回测与模拟盘不一致"在结构上
//! 不可能发生。

pub mod fill;
pub mod liquidity;
pub mod metrics;

pub use fill::{
    FillContext, FillModel, FillOutcome, M0WickTouchFull, M1TradeThroughQueue, MODEL_NAMES,
    Optimism, model_by_name,
};
pub use liquidity::{Trade, TradeTape};
pub use metrics::{
    BacktestProvenance, EdgeMetrics, FeeHonesty, GapSummary, LatencyStats, MarkoutObservation,
    MarkoutSide, MarkoutSummary, StopExposure, StopExposureSummary, StopResolution,
    summarize_markouts, summarize_stop_exposures,
};
