//! 交易域核心类型与状态机。
//!
//! 本 crate 的硬约束：**零 I/O**。不允许依赖 tokio、reqwest、duckdb、axum，
//! 不允许读写文件、网络或系统时钟。所有外部输入都通过参数传入。
//!
//! 这条约束换来的能力：回测、模拟盘、实盘三种模式共用同一套订单生命周期与
//! 保护单数学，并且全部可用纯函数测试覆盖。旧实现把止盈价计算复制在三处
//! （backtest/paper/live）并已发生分叉，本 crate 的存在就是为了让那类 bug
//! 无法再发生。

pub mod error;
pub mod instrument;
pub mod money;
pub mod order;
pub mod precision;
pub mod protection;
pub mod state;

pub use error::{DomainError, ExchangeError, RejectReason};
pub use instrument::{ContractKind, FeeSchedule, FeeSource, Instrument};
pub use money::{Price, Qty};
pub use order::{
    ClientOrderId, Effect, HaltReason, Order, OrderPurpose, OrderState, Side, TimeInForce,
};
pub use precision::{Precision, PriceRole};
pub use protection::{
    BreakEvenSpec, EntryFill, MarketSlice, ProtectionAction, ProtectionPlan, ProtectionPlanner,
    StopSpec, TpPlan, TpRung, TrailingSpec, actions_to_effects,
};
pub use state::{ExecEvent, Fill, OrderBookState, Position, QueryOutcome, TrackedOrder};
