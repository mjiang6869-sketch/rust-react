//! HTTP + WebSocket API。
//!
//! # 职责边界
//!
//! 本 crate **不做交易决策**。它接收请求、转换成领域类型、委托给 `engine`，
//! 再把结果序列化返回。所有业务逻辑在 `domain` / `sim` / `engine` 里。
//!
//! # 两条传输层约束
//!
//! ## 1. Decimal 一律序列化为字符串
//!
//! JavaScript 的 `number` 是 IEEE 754 双精度，`3200.12345678` 会在前端丢精度。
//! 而止盈目标是 bp 级——丢精度会让界面显示的价与后端将挂出的价不一致，
//! 用户看到的不是即将发生的事。
//!
//! ## 2. 拒绝必须带可读的中文原因
//!
//! 风控拒绝、参数错误、数据缺失都返回面向用户的说明，而不只是错误码。
//! 静默失败是恶劣的失败模式——用户会以为策略不工作而不知道原因。
//!
//! # 路由概览
//!
//! - `GET  /api/v1/health`、`/state`、`/instrument`、`/strategies`、`/fill-models`
//! - `POST /api/v1/manual/preview`、`/submit`、`/cancel-pending`、`/close`
//! - `GET  /api/v1/auto-maker`、`PUT /api/v1/auto-maker`（自动化做市开关与参数）
//! - `GET  /api/v1/orders`、`/fills`、`/pnl`、`/backtests`
//! - `POST /api/v1/backtest`
//! - `GET  /api/v1/data/coverage`、`POST /api/v1/data/download`
//! - `POST /api/v1/live/arm`、`/disarm`、`PUT /api/v1/mode`
//! - `GET  /api/v1/market/klines`、`/market/book`
//! - `GET  /api/v1/ws`（引擎状态）、`/api/v1/market/stream`（盘口与最新价推送）

pub mod auto_maker;
pub mod backtest;
pub mod download_task;
pub mod dto;
pub mod engine_feed;
pub mod market_stream;
pub mod overview;
pub mod routes;
pub mod state;
pub mod ws;

pub use dto::{ApiError, ApiResponse, ManualPlanDto, ManualPreviewDto, StateDto};
pub use routes::router;
pub use state::{AppState, ProgressMessage};
