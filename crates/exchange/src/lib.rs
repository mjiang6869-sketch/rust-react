//! 币安 USDⓈ-M 合约客户端：签名、合约规则解析、下单请求构造。
//!
//! # 本 crate 处理的两个币安特性，直接决定能不能做市
//!
//! ## 1. 没有 OCO / bracket
//!
//! 支持的条件单只有 `STOP / STOP_MARKET / TAKE_PROFIT / TAKE_PROFIT_MARKET /
//! TRAILING_STOP_MARKET`，一张单只能对应一个数量、一个触发价。所以"一个点位
//! 挂单 + 分批止盈 + 止损"必须由 `domain::position_set` 自己管理。
//!
//! ## 2. `triggerProtect`：条件单触发价必须距标记价至少 5%
//!
//! 做市的止损往往就在市价附近几个基点，会被这条规则直接拒单。所以：
//! - 入场用 **GTX 限价单**，不受约束
//! - 止损在近价位时用**限价单**挂出，而非条件单
//! - 只有远离市价的止损才适合 `STOP` 条件单
//!
//! `binance::check_trigger_protect` 把这个判断显式化，让调用方能选择退化路径。
//!
//! # 安全边界
//!
//! - endpoint 白名单硬编码，只允许币安官方生产与测试网域名
//! - 密钥不会被 `Debug` 打印，不会出现在签名结果或错误信息里

pub mod binance;
pub mod client;
pub mod error;
pub mod signing;

pub use binance::{
    AcceptedOrder, AccountFeeResponse, BinanceError, ContractSpec, ExchangeInfoResponse,
    OrderRequest, OrderResponse, RawSymbol, TriggerProtectVerdict, check_trigger_protect,
    classify_api_error, fee_schedule_from_account, find_contract, parse_all_contracts,
    parse_contract, reject_reason_from_code, side_tag,
};
pub use client::{
    BinanceClient, Mode, PRODUCTION_URL, TESTNET_URL, exchange_mode_for, parse_available_balance,
};
pub use error::ExchangeError;
pub use signing::{Credentials, SignError, endpoint_allowed, sign, signed_query, timestamp_ms};
