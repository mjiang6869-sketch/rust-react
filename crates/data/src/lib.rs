//! 历史行情数据层：下载、校验、转换、台账。
//!
//! # 数据来源与可用性（实测确认，2026-09）
//!
//! 全部走 `data.binance.vision` 归档（CDN 静态 ZIP，支持 HTTP Range 断点续传，
//! 无速率限制），而不是 REST——用 REST 拉多年历史会撞上权重限制并有封禁风险。
//!
//! | 数据集 | 覆盖 | 用途 |
//! | --- | --- | --- |
//! | `klines/1m` | 2024-01 起完整 | 策略信号 |
//! | `aggTrades` | 2024-01 起完整 | **M1 成交模型 + markout** |
//! | `markPriceKlines/1m` | 2024-01 起完整 | 强平距离 |
//! | `fundingRate` | 完整 | 持仓成本 |
//! | `trades` | 完整 | **不使用**：字段与 aggTrades 同构，仅体积 2.5 倍 |
//! | `bookTicker` | **2024-04 后停更** | 不可用。markout 改用 aggTrades 成交价 |
//!
//! # 归档的两个坑（实测踩到）
//!
//! 1. `klines` 与 `markPriceKlines` 解压后**得到同名 CSV**，必须按数据集分目录
//!    解压，否则标记价会静默覆盖真实 K 线。
//! 2. S3 的 `HEAD` 响应不带 `content-length`，判断存在性与大小要用带 Range 的
//!    `GET`。
//!
//! # 两条不变量
//!
//! 1. **排队位置无法从任何免费归档还原。** `aggTrades` 与 `trades` 都只有
//!    `is_buyer_maker` 一个方向标志，没有买卖双方的订单 ID。所以 M1 成交模型
//!    "假设我们排在队尾"不是保守选择，而是唯一可能的选择。
//! 2. **缺口必须阻断回测。** 跨越缺口会凭空发明不可能的成交。缺口记录在台账里，
//!    回测必须在存在缺口时拒绝运行。

pub mod fixed;
pub mod manifest;
pub mod parquet_writer;
pub mod replay;

pub use fixed::{decode_price, decode_rate, encode_price, encode_rate, parse_decimal};
pub use manifest::{
    CURRENT_SCHEMA_VERSION, DatasetKind, Gap, Manifest, PartitionEntry, PartitionKey,
    PartitionStatus, archive_url_and_name, months_between,
};
pub use parquet_writer::{BATCH_ROWS, ConversionStats, convert_csv_to_parquet};
pub use replay::{
    DaySlice, MissingDataPolicy, NoProgress, ReplayOutput, ReplayProgress, ReplaySpec,
    agg_trades_parquet_path, days_in_month, ensure_parquet, klines_parquet_path, load_day,
};
