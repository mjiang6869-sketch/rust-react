//! 公开行情客户端：K 线、盘口。
//!
//! # 与 `client.rs` 的分工
//!
//! `client.rs` 处理**签名**请求（下单、账户）。本模块处理**公开**数据——
//! 不需要 API Key，所以也不需要 endpoint 白名单之外的额外约束（但白名单仍
//! 生效，防止配置被改到任意地址）。
//!
//! # 数据来源的边界
//!
//! 这里只走 REST，用于**补齐历史与初始快照**。实时推送走 WebSocket
//! （见 `stream.rs`）——REST 轮询做实时更新既浪费配额又慢。最近成交完全
//! 不走 REST：那条接口权重 20，是全项目最贵的公开接口，而成交流只服务
//! 一个已经由 WebSocket 覆盖的场景（见 `stream.rs` 的 `MarketView::trades`）。
//!
//! # TTL 是按"权重"定的，不是按"新鲜度"定的
//!
//! 币安的额度按**权重**算，且按 IP 计（2400/分钟）。各接口权重差别极大：
//!
//! | 接口 | 权重 | 说明 |
//! |---|---|---|
//! | `/fapi/v1/depth` | 2（limit≤50）/ 5（≤100）/ 10（≤500） | 便宜 |
//! | `/fapi/v1/klines` | 1（<100 根）/ 2（<500）/ 5（500–1000）/ 10（>1000） | 便宜 |

use chrono::{DateTime, TimeZone, Utc};
use domain::{BookSnapshot, Candle, Price};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::client::BinanceClient;
use crate::error::ExchangeError;

/// 盘口快照的缓存时长：1 秒。
///
/// 权重只有 2，是最便宜的接口，可以让它保持接近实时。1 秒也正好对齐
/// 事故里前端的轮询间隔——也就是说**同样的界面刷新频率，上游请求量降到
/// 前端数量的倒数**。
pub const DEPTH_TTL_MS: i64 = 1_000;

/// K 线的缓存时长：2 秒。
///
/// 权重 5（500 根）。K 线的最小周期是 1 分钟，2 秒的缓存对图表是
/// 无感的——但注意它只适用于**最后一根在变**的场景，历史翻页见
/// [`HISTORY_TTL_MS`]。
pub const KLINES_TTL_MS: i64 = 2_000;

/// 历史 K 线（带 `endTime` 的翻页）的缓存时长：5 分钟。
///
/// **已收盘的历史 K 线不会变**。翻页请求重复打上游纯属浪费，而且用户往
/// 回滚动的行为模式天然会造成同一段区间被反复请求。5 分钟足够覆盖一次
/// 连续滚动的全过程。
pub const HISTORY_TTL_MS: i64 = 300_000;

/// 标记价的缓存时长：1 秒。
///
/// 权重 1。用于估算强平距离，不需要亚秒级精度。
pub const MARK_PRICE_TTL_MS: i64 = 1_000;

/// K 线周期。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Interval {
    M1,
    M3,
    M5,
    M15,
    M30,
    H1,
    H4,
    D1,
}

impl Interval {
    /// 币安 API 的周期标识。
    pub fn as_str(self) -> &'static str {
        match self {
            Interval::M1 => "1m",
            Interval::M3 => "3m",
            Interval::M5 => "5m",
            Interval::M15 => "15m",
            Interval::M30 => "30m",
            Interval::H1 => "1h",
            Interval::H4 => "4h",
            Interval::D1 => "1d",
        }
    }

    /// 从字符串解析。只接受精确匹配——模糊匹配会让拼错的周期静默变成默认值。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "1m" => Some(Interval::M1),
            "3m" => Some(Interval::M3),
            "5m" => Some(Interval::M5),
            "15m" => Some(Interval::M15),
            "30m" => Some(Interval::M30),
            "1h" => Some(Interval::H1),
            "4h" => Some(Interval::H4),
            "1d" => Some(Interval::D1),
            _ => None,
        }
    }

    /// 一根 K 线的毫秒数。用于判断"当前 K 线"与聚合。
    pub fn millis(self) -> i64 {
        match self {
            Interval::M1 => 60_000,
            Interval::M3 => 180_000,
            Interval::M5 => 300_000,
            Interval::M15 => 900_000,
            Interval::M30 => 1_800_000,
            Interval::H1 => 3_600_000,
            Interval::H4 => 14_400_000,
            Interval::D1 => 86_400_000,
        }
    }
}

/// 币安 K 线响应的一行（数组形式）。
///
/// 币安返回的是位置数组而非对象，所以用元组反序列化。字段顺序是接口契约的
/// 一部分，注释标注了每一列的含义以防错位。
#[derive(Debug, Deserialize)]
#[serde(from = "Vec<serde_json::Value>")]
pub struct KlineRow {
    pub open_time: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub close_time: i64,
    pub trade_count: i64,
}

impl From<Vec<serde_json::Value>> for KlineRow {
    fn from(v: Vec<serde_json::Value>) -> Self {
        // 币安的 kline 数组位置：
        //   [0] 开盘时间  [1] 开  [2] 高  [3] 低  [4] 收  [5] 成交量
        //   [6] 收盘时间  [7] 成交额  [8] 成交笔数  [9] 主动买量 ...
        let dec = |i: usize| -> Decimal {
            v.get(i)
                .and_then(|x| match x {
                    serde_json::Value::String(s) => s.parse::<Decimal>().ok(),
                    serde_json::Value::Number(n) => n.to_string().parse::<Decimal>().ok(),
                    _ => None,
                })
                .unwrap_or(Decimal::ZERO)
        };
        let int = |i: usize| -> i64 {
            v.get(i)
                .and_then(|x| match x {
                    serde_json::Value::Number(n) => n.as_i64(),
                    serde_json::Value::String(s) => s.parse::<i64>().ok(),
                    _ => None,
                })
                .unwrap_or(0)
        };
        Self {
            open_time: int(0),
            open: dec(1),
            high: dec(2),
            low: dec(3),
            close: dec(4),
            volume: dec(5),
            close_time: int(6),
            trade_count: int(8),
        }
    }
}

impl KlineRow {
    /// 转成领域层的 `Candle`。
    ///
    /// `closed` 由**收盘时间**判断，不用"当前 K 线就是最后一根"这类推测：
    /// 币安 REST 返回的最后一根可能是未收盘的当前 K 线，而**策略不能使用
    /// 未收盘 K 线**（那等于偷看未来）。
    ///
    /// 这里用币安给的 `close_time` 而不是 `open_time + interval` 推算——
    /// 交易所的数据比我们推的准，而且省掉一个可以传错的参数。
    pub fn to_candle(&self, now: DateTime<Utc>) -> Candle {
        let close_time = Utc
            .timestamp_millis_opt(self.close_time)
            .single()
            .unwrap_or(now);
        Candle {
            open_time: Utc
                .timestamp_millis_opt(self.open_time)
                .single()
                .unwrap_or(now),
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            // 收盘时间已过 => 这根 K 线已闭合
            closed: now >= close_time,
        }
    }

    /// 该 K 线的结束时刻（毫秒）。
    pub fn close_millis(&self, interval: Interval) -> i64 {
        self.open_time + interval.millis() - 1
    }
}

impl BinanceClient {
    /// 拉取历史 K 线。
    ///
    /// `limit` 上限 1500（币安限制）。`end_time` 用于向过去翻页——
    /// 币安不提供 offset，只能靠时间游标。
    pub async fn klines(
        &self,
        symbol: &str,
        interval: Interval,
        limit: u32,
    ) -> Result<Vec<Candle>, ExchangeError> {
        self.klines_before(symbol, interval, limit, None).await
    }

    /// 拉取 `end_time` 之前的 K 线（用于翻页加载更早的数据）。
    pub async fn klines_before(
        &self,
        symbol: &str,
        interval: Interval,
        limit: u32,
        end_time_ms: Option<i64>,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let limit = limit.clamp(1, 1500);
        let mut query = format!(
            "symbol={symbol}&interval={}&limit={limit}",
            interval.as_str()
        );
        // 带 `endTime` 的是历史翻页：结果区间已经固定，不会变；不带的是
        // "最近 limit 根"，最后一根还在长。用不同的 TTL。
        let ttl = if let Some(t) = end_time_ms {
            query.push_str(&format!("&endTime={t}"));
            HISTORY_TTL_MS
        } else {
            KLINES_TTL_MS
        };

        let body = self
            .get_public_cached("/fapi/v1/klines", &query, ttl)
            .await
            .map_err(|e| match e {
                // 合约不存在是配置问题，不是临时故障——归为致命错误避免重试
                ExchangeError::Definitive(m) if m.contains("-1121") => {
                    ExchangeError::Fatal(format!("合约不存在或不在交易中：{symbol}"))
                }
                other => other,
            })?;

        let rows: Vec<KlineRow> = serde_json::from_str(&body)
            .map_err(|e| ExchangeError::Fatal(format!("K 线响应无法解析：{e}")))?;

        let now = Utc::now();
        Ok(rows.iter().map(|r| r.to_candle(now)).collect())
    }

    /// 拉取盘口快照（用于界面右侧的深度）。
    pub async fn depth(&self, symbol: &str, limit: u32) -> Result<BookSnapshot, ExchangeError> {
        let limit = match limit {
            0..=5 => 5,
            6..=10 => 10,
            11..=20 => 20,
            21..=50 => 50,
            51..=100 => 100,
            101..=500 => 500,
            _ => 1000,
        };
        let body = self
            .get_public_cached(
                "/fapi/v1/depth",
                &format!("symbol={symbol}&limit={limit}"),
                DEPTH_TTL_MS,
            )
            .await?;

        #[derive(Debug, Deserialize)]
        struct Raw {
            bids: Vec<[String; 2]>,
            asks: Vec<[String; 2]>,
        }

        let raw: Raw = serde_json::from_str(&body)
            .map_err(|e| ExchangeError::Fatal(format!("盘口响应无法解析：{e}")))?;

        let parse_side = |v: &[[String; 2]]| -> Vec<(Decimal, Decimal)> {
            v.iter()
                .filter_map(|pair| {
                    let p = pair[0].parse::<Decimal>().ok()?;
                    let q = pair[1].parse::<Decimal>().ok()?;
                    Some((p, q))
                })
                .collect()
        };

        let bids = parse_side(&raw.bids);
        let asks = parse_side(&raw.asks);
        let bid = bids.first().map(|(p, _)| *p).unwrap_or(Decimal::ZERO);
        let ask = asks.first().map(|(p, _)| *p).unwrap_or(Decimal::ZERO);

        Ok(BookSnapshot {
            bid,
            ask,
            bids,
            asks,
            at: Utc::now(),
        })
    }

    /// 拉取标记价。用于估算强平距离。
    pub async fn mark_price(&self, symbol: &str) -> Result<Price, ExchangeError> {
        let body = self
            .get_public_cached(
                "/fapi/v1/premiumIndex",
                &format!("symbol={symbol}"),
                MARK_PRICE_TTL_MS,
            )
            .await?;

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            #[serde(default)]
            mark_price: String,
        }

        let raw: Raw = serde_json::from_str(&body)
            .map_err(|e| ExchangeError::Fatal(format!("标记价响应无法解析：{e}")))?;
        Ok(Price::new(raw.mark_price.parse().unwrap_or(Decimal::ZERO)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn interval_round_trips() {
        for iv in [
            Interval::M1,
            Interval::M5,
            Interval::M15,
            Interval::H1,
            Interval::D1,
        ] {
            assert_eq!(Interval::parse(iv.as_str()), Some(iv));
        }
    }

    /// 只接受精确匹配——拼错的周期不能静默变成默认值。
    #[test]
    fn interval_parsing_is_strict() {
        assert_eq!(Interval::parse("1min"), None);
        assert_eq!(Interval::parse("1M"), None, "大小写敏感");
        assert_eq!(Interval::parse("2h"), None);
        assert_eq!(Interval::parse(""), None);
    }

    #[test]
    fn interval_millis_are_correct() {
        assert_eq!(Interval::M1.millis(), 60_000);
        assert_eq!(Interval::M15.millis(), 900_000);
        assert_eq!(Interval::H1.millis(), 3_600_000);
        assert_eq!(Interval::D1.millis(), 86_400_000);
    }

    /// 币安返回的是位置数组，字段顺序是接口契约的一部分。
    #[test]
    fn kline_row_parses_binance_array_format() {
        let raw: Vec<serde_json::Value> = serde_json::from_str(
            r#"["1790354340000","2687.41","2689.23","2687.33","2688.66","291.408",
                "1790354399999","783377.68213",916,"198.946","534820.76710","0"]"#,
        )
        .unwrap();
        let row = KlineRow::from(raw);

        assert_eq!(row.open_time, 1_790_354_340_000);
        assert_eq!(row.open, dec!(2687.41));
        assert_eq!(row.high, dec!(2689.23));
        assert_eq!(row.low, dec!(2687.33));
        assert_eq!(row.close, dec!(2688.66));
        assert_eq!(row.volume, dec!(291.408));
        assert_eq!(row.close_time, 1_790_354_399_999);
        assert_eq!(row.trade_count, 916);
    }

    /// 最后一根 K 线是否已收盘由收盘时间判断——策略不能使用未收盘 K 线。
    #[test]
    fn last_candle_closed_flag_follows_close_time() {
        let row = KlineRow {
            open_time: 1_790_354_340_000,
            open: dec!(2687),
            high: dec!(2690),
            low: dec!(2686),
            close: dec!(2688),
            volume: dec!(100),
            close_time: 1_790_354_399_999,
            trade_count: 10,
        };

        // 在收盘时间之后观察 -> 已闭合
        let after = Utc.timestamp_millis_opt(1_790_354_400_000).unwrap();
        assert!(row.to_candle(after).closed);

        // 在收盘时间之前观察 -> 未闭合
        let during = Utc.timestamp_millis_opt(1_790_354_370_000).unwrap();
        assert!(
            !row.to_candle(during).closed,
            "未收盘的 K 线必须标记出来，策略不能使用它"
        );
    }

    #[test]
    fn kline_row_tolerates_missing_optional_fields() {
        // 只有前 6 个字段（截断的响应）
        let raw: Vec<serde_json::Value> =
            serde_json::from_str(r#"["1000","1.5","2.5","1.0","2.0","10"]"#).unwrap();
        let row = KlineRow::from(raw);
        assert_eq!(row.open, dec!(1.5));
        assert_eq!(row.trade_count, 0, "缺失字段按 0 处理而不 panic");
    }

    #[test]
    fn kline_row_accepts_numbers_not_just_strings() {
        // 某些情况下币安返回数字而非字符串
        let raw: Vec<serde_json::Value> =
            serde_json::from_str(r#"[1000, 1.5, 2.5, 1.0, 2.0, 10, 1999, 1, 5]"#).unwrap();
        let row = KlineRow::from(raw);
        assert_eq!(row.open, dec!(1.5));
        assert_eq!(row.open_time, 1000);
    }

    #[test]
    fn close_millis_uses_interval_length() {
        let row = KlineRow {
            open_time: 1_000_000,
            open: dec!(1),
            high: dec!(1),
            low: dec!(1),
            close: dec!(1),
            volume: dec!(1),
            close_time: 0,
            trade_count: 0,
        };
        assert_eq!(row.close_millis(Interval::M1), 1_059_999);
        assert_eq!(row.close_millis(Interval::H1), 4_599_999);
    }

    /// 无效数字不能 panic——行情数据偶有异常，服务不该因此崩溃。
    #[test]
    fn kline_row_handles_invalid_numbers() {
        let raw: Vec<serde_json::Value> =
            serde_json::from_str(r#"["1000","abc","2.5","1.0","2.0","10"]"#).unwrap();
        let row = KlineRow::from(raw);
        assert_eq!(row.open, Decimal::ZERO, "无效值退化为 0 而非 panic");
        assert_eq!(row.high, dec!(2.5), "其他字段不受影响");
    }
}
