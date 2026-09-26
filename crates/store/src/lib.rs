//! 热状态持久化：订单、成交、持仓、回测记录、AI 会话、图表标注。
//!
//! # 为什么用 SQLite 而不是 JSON 文件
//!
//! 旧实现把整个状态序列化成一个 JSON，每次订单变动都重写整个文件。问题：
//!
//! 1. 订单历史会无限增长，重写成本随之上升
//! 2. 无法按时间范围查询（交易总览需要）
//! 3. AI 会话与分析历史没有合适的存放处
//! 4. 崩溃时的写入窗口远大于单行插入
//!
//! SQLite 的 WAL 模式让"写入订单"与"查询状态"不互相阻塞，且单条记录插入
//! 的崩溃窗口极小。这对交易系统很重要——崩溃丢掉刚成交的记录会导致持仓
//! 对账错误。
//!
//! # 与冷数据的边界
//!
//! 本 crate **只**管热状态（行数少、需要事务）。历史行情（K 线、逐笔成交，
//! 数百万行）走 `data` crate 的 Parquet + DuckDB——那边需要列剪枝与谓词
//! 下推，不是事务。两者混在一个引擎里会让任一边都做不好。

pub mod equity;
pub mod history;
pub mod orders;
pub mod schema;

use thiserror::Error;

pub use equity::{EquitySample, insert_equity_sample, recent_equity_samples};
pub use history::{
    AiSessionRow, AnnotationRow, BacktestRunRow, IntegrityReport, PnlSummary, ai_session_context,
    delete_annotation, get_backtest_run, has_open_orders, insert_ai_session, insert_backtest_run,
    integrity_check, list_annotations, pnl_summary, prune_backtest_runs, recent_ai_sessions,
    recent_backtest_runs, upsert_annotation,
};
pub use orders::{
    FillRow, OrderRow, PositionRow, PositionUpdate, add_realized_pnl, dec_from_sql, dec_to_sql,
    fills_in_range, insert_fill, load_position, orders_needing_reconciliation, realized_pnl,
    recent_orders, upsert_order, upsert_position,
};
pub use schema::{CURRENT_VERSION, configure, migrate, version};

/// 持久化错误。
///
/// 全部是显式的——本层不做"吞掉错误继续"的处理。交易系统的持久化失败必须
/// 让调用方知道，否则会出现"内存里成交了但没落盘"的不一致。
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("数据库错误：{0}")]
    Database(#[from] rusqlite::Error),
    #[error("无法解析金额或数量：{0}")]
    InvalidDecimal(String),
    #[error("非法时间戳：{0}")]
    InvalidTimestamp(i64),
    #[error("未知的枚举标签：{0}")]
    InvalidTag(String),
    #[error("引用了不存在的订单：{0}")]
    UnknownOrder(String),
}
