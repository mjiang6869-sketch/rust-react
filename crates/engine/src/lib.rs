//! 编排层：把行情、策略、撮合、持久化与交易所串起来。
//!
//! # 职责边界
//!
//! 本 crate **不定义新类型**，也**不含价格计算**。它只做三件事：
//!
//! 1. 决定每个模式用哪个数据源与哪个执行适配器
//! 2. 驱动事件循环（喂行情 → 取意图 → 过风控 → 编译订单 → 判定成交）
//! 3. 在状态变更时调用 `store` 落盘
//!
//! 所有类型来自 `domain`，所有成交判定来自 `sim`，所有持久化来自 `store`。
//! 这条边界让"回测与实盘不一致"在结构上不可能发生——它们走的是同一套
//! `OrderBookState` 与 `ProtectionPlanner`，只是数据源不同。
//!
//! # 模拟盘与回测的关系
//!
//! **模拟盘就是喂实时数据的回测。** 两者用同一个 `sim::FillModel` 判定成交，
//! 同一个 `domain::position_set` 管保护单。差别只在数据来源：回测读 Parquet，
//! 模拟盘读 WebSocket。
//!
//! 这条设计是刻意的：一旦给模拟盘写一套独立的成交逻辑，就会重现旧项目
//! "三套状态机互相分叉"的问题。

pub mod paper;

pub use paper::{EngineConfig, EngineEvent, EngineSnapshot, PaperEngine, SubmitOutcome};
