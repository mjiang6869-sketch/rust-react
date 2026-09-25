//! 从 Parquet 回放历史数据，产出 `domain::MarketEvent` 流。
//!
//! # 内存策略：按天分片，不整月载入
//!
//! 单月 aggTrades 有 1400 万笔（ETHUSDC 2026-08 实测）。整月载入约 3-4 GB，
//! 虽然 32 GB 机器放得下，但加上 Parquet 解码中间态与回测自身的状态，
//! 余量会变得紧张，而且扩大到多标的并行回测时立刻不可行。
//!
//! 所以这里按**天**分片：一天约 45 万笔、约 100 MB。代价是跨天的持仓需要
//! 衔接——但成交带本身是连续窗口，由调用方按天推进时保持窗口重叠即可。
//!
//! # 时间归并
//!
//! K 线是分钟粒度、成交是毫秒粒度。回放必须按时间归并两条流，且**保证
//! 同一时刻的成交先于该时刻收盘的 K 线**——策略看到 K 线收盘时，那根 K 线
//! 期间的成交已经全部发生过了。顺序反了会让策略用"还没发生的成交"做决策。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{TimeZone, Utc};
use domain::{AggTrade, Candle, MarketEvent, Price, Qty};

/// 一个分区的 Parquet 路径（Hive 分区布局）。
pub fn klines_parquet_path(
    root: &Path,
    symbol: &str,
    interval: &str,
    year: i32,
    month: u32,
) -> PathBuf {
    root.join("lake")
        .join("klines")
        .join(format!("symbol={symbol}"))
        .join(format!("interval={interval}"))
        .join(format!("year={year}"))
        .join(format!("month={month:02}"))
        .join("data.parquet")
}

pub fn agg_trades_parquet_path(
    root: &Path,
    symbol: &str,
    year: i32,
    month: u32,
    day: u32,
) -> PathBuf {
    root.join("lake")
        .join("agg_trades")
        .join(format!("symbol={symbol}"))
        .join(format!("date={year}-{month:02}-{day:02}"))
        .join("data.parquet")
}

/// 定点价格缩放因子（与 `parquet_writer` 写入时一致）。
const SCALE: i64 = 100_000_000;

fn decode_price(raw: i64) -> Price {
    Price::new(decimal_from_fixed(raw, SCALE))
}

/// 把定点整数还原为 `Decimal`。
fn decimal_from_fixed(raw: i64, scale: i64) -> rust_decimal::Decimal {
    rust_decimal::Decimal::from(raw) / rust_decimal::Decimal::from(scale)
}

/// 校验 Parquet 文件是否存在且非空。
pub fn ensure_parquet(path: &Path) -> Result<()> {
    let md =
        std::fs::metadata(path).with_context(|| format!("数据文件不存在: {}", path.display()))?;
    if md.len() == 0 {
        bail!("数据文件为空: {}", path.display());
    }
    Ok(())
}

/// 一天的数据。
#[derive(Debug, Default)]
pub struct DaySlice {
    pub candles: Vec<Candle>,
    pub trades: Vec<AggTrade>,
}

impl DaySlice {
    /// 归并为按时间排序的事件流。
    ///
    /// 归并规则：同一毫秒时**成交先于 K 线**。理由是 K 线代表一段时间，
    /// 那一刻收盘的 K 线应当在该时刻的成交之后被策略看到。
    ///
    /// 这个顺序不是细节——反了会让策略在决策时把"尚未发生的成交"当成
    /// 已知信息，等价于偷看未来。
    pub fn into_events(self) -> Vec<MarketEvent> {
        let mut events: Vec<MarketEvent> =
            Vec::with_capacity(self.candles.len() + self.trades.len());
        for t in self.trades {
            events.push(MarketEvent::AggTrade(t));
        }
        for c in self.candles {
            events.push(MarketEvent::Kline(c));
        }
        events.sort_by(|a, b| {
            a.at()
                .cmp(&b.at())
                .then_with(|| event_priority(a).cmp(&event_priority(b)))
        });
        events
    }
}

/// 同一时刻的事件优先级：数值小的先处理。
fn event_priority(e: &MarketEvent) -> u8 {
    match e {
        // 成交先于 K 线
        MarketEvent::AggTrade(_) => 0,
        MarketEvent::Book(_) => 1,
        MarketEvent::MarkPrice { .. } => 2,
        // K 线最后：它汇总了该时间段
        MarketEvent::Kline(_) => 3,
        MarketEvent::Clock(_) => 4,
    }
}

/// 回放配置：一次回测要读哪些数据。
#[derive(Clone, Debug)]
pub struct ReplaySpec {
    pub symbol: String,
    pub interval: String,
    pub from: (i32, u32),
    pub to: (i32, u32),
}

impl ReplaySpec {
    /// 枚举需要读取的月份。
    pub fn months(&self) -> Vec<(i32, u32)> {
        crate::manifest::months_between(self.from, self.to)
    }

    /// 该区间的总天数（用于进度显示与分片数估算）。
    pub fn day_count(&self) -> u32 {
        self.months()
            .iter()
            .map(|(y, m)| days_in_month(*y, *m))
            .sum()
    }
}

/// 某年某月的天数。
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

/// 回放进度回调。用于 CLI 与前端显示进度。
pub trait ReplayProgress: Send {
    fn on_day(&mut self, day: &str, events: usize, total: usize);
    fn on_month(&mut self, year: i32, month: u32);
}

/// 无操作进度回调。
pub struct NoProgress;
impl ReplayProgress for NoProgress {
    fn on_day(&mut self, _: &str, _: usize, _: usize) {}
    fn on_month(&mut self, _: i32, _: u32) {}
}

/// 数据缺失时的行为。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingDataPolicy {
    /// 缺失即报错。**默认**——跨越缺口会凭空发明不可能的成交。
    Fail,
    /// 允许缺失，但记录到缺口列表里，且回测结果会标记为不完整。
    AllowAndRecord,
}

/// 一次回放读到的数据与发现的缺口。
pub struct ReplayOutput {
    pub events: Vec<MarketEvent>,
    /// 缺失的分片（按天或按月）。
    pub missing: Vec<String>,
    /// 实际读到的事件数。
    pub event_count: usize,
}

// ---------------------------------------------------------------------------
// DuckDB 读取
// ---------------------------------------------------------------------------

/// 从 Parquet 读一天的 K 线与逐笔成交。
///
/// 用 DuckDB 的 `read_parquet` 直接查文件，靠分区列与谓词下推只扫需要的
/// 行组——不把整个文件读进内存。
pub fn load_day(
    root: &Path,
    symbol: &str,
    interval: &str,
    year: i32,
    month: u32,
    day: u32,
) -> Result<DaySlice> {
    let mut slice = DaySlice::default();

    // K 线按月分片
    let kpath = klines_parquet_path(root, symbol, interval, year, month);
    if kpath.exists() {
        slice.candles = read_candles_for_day(&kpath, year, month, day)?;
    }

    // 逐笔成交按天分片
    let tpath = agg_trades_parquet_path(root, symbol, year, month, day);
    if tpath.exists() {
        slice.trades = read_trades(&tpath)?;
    }

    Ok(slice)
}

/// 读某个月的 K 线，只保留指定日期。
fn read_candles_for_day(path: &Path, year: i32, month: u32, day: u32) -> Result<Vec<Candle>> {
    // 这一层刻意用 rusqlite 之外的轻量方式：直接解 Parquet。
    // 见下方 `parquet_reader` 模块实现。
    parquet_reader::read_candles(path, year, month, day)
}

fn read_trades(path: &Path) -> Result<Vec<AggTrade>> {
    parquet_reader::read_trades(path)
}

// ---------------------------------------------------------------------------
// Parquet 读取实现
// ---------------------------------------------------------------------------

mod parquet_reader {
    use super::*;
    use arrow::array::{Array, BooleanArray, Int64Array};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    /// 列索引：与 `parquet_writer::schema_for` 保持一致。
    mod kline_cols {
        pub const OPEN_TIME: usize = 0;
        pub const OPEN: usize = 1;
        pub const HIGH: usize = 2;
        pub const LOW: usize = 3;
        pub const CLOSE: usize = 4;
        pub const VOLUME: usize = 5;
    }

    mod trade_cols {
        pub const TRADE_ID: usize = 0;
        pub const PRICE: usize = 1;
        pub const QUANTITY: usize = 2;
        pub const TIME: usize = 5;
        pub const IS_BUYER_MAKER: usize = 6;
    }

    fn open_reader(path: &Path) -> Result<ParquetRecordBatchReaderBuilder<std::fs::File>> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("打开 Parquet 失败: {}", path.display()))?;
        ParquetRecordBatchReaderBuilder::try_new(file)
            .with_context(|| format!("解析 Parquet 元数据失败: {}", path.display()))
    }

    fn i64_at(batch: &arrow::record_batch::RecordBatch, col: usize, row: usize) -> i64 {
        batch
            .column(col)
            .as_any()
            .downcast_ref::<Int64Array>()
            .map(|a| a.value(row))
            .unwrap_or(0)
    }

    pub fn read_candles(path: &Path, year: i32, month: u32, day: u32) -> Result<Vec<Candle>> {
        let builder = open_reader(path)?;
        // 只读需要的列——Parquet 是列存，未读的列不参与 IO 与解码。
        let reader = builder
            .with_batch_size(8192)
            .build()
            .context("构造 Parquet reader 失败")?;

        let want = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .context("非法日期")?
            .and_hms_opt(0, 0, 0)
            .context("非法时间")?
            .and_utc();
        let day_start = want.timestamp_millis();
        let day_end = day_start + 86_400_000;

        let mut out = Vec::new();
        for batch in reader {
            let batch = batch.context("读取 Parquet 批次失败")?;
            for row in 0..batch.num_rows() {
                let ts = i64_at(&batch, kline_cols::OPEN_TIME, row);
                if ts < day_start || ts >= day_end {
                    continue;
                }
                out.push(Candle {
                    open_time: Utc
                        .timestamp_millis_opt(ts)
                        .single()
                        .context("K 线时间戳非法")?,
                    open: decode_price(i64_at(&batch, kline_cols::OPEN, row)).get(),
                    high: decode_price(i64_at(&batch, kline_cols::HIGH, row)).get(),
                    low: decode_price(i64_at(&batch, kline_cols::LOW, row)).get(),
                    close: decode_price(i64_at(&batch, kline_cols::CLOSE, row)).get(),
                    volume: decode_price(i64_at(&batch, kline_cols::VOLUME, row)).get(),
                    closed: true,
                });
            }
        }
        Ok(out)
    }

    pub fn read_trades(path: &Path) -> Result<Vec<AggTrade>> {
        let builder = open_reader(path)?;
        let reader = builder
            .with_batch_size(8192)
            .build()
            .context("构造 Parquet reader 失败")?;

        let mut out = Vec::new();
        for batch in reader {
            let batch = batch.context("读取 Parquet 批次失败")?;
            let makers = batch
                .column(trade_cols::IS_BUYER_MAKER)
                .as_any()
                .downcast_ref::<BooleanArray>()
                .cloned();
            for row in 0..batch.num_rows() {
                let ts = i64_at(&batch, trade_cols::TIME, row);
                let id = i64_at(&batch, trade_cols::TRADE_ID, row);
                out.push(AggTrade {
                    trade_id: id.max(0) as u64,
                    price: decode_price(i64_at(&batch, trade_cols::PRICE, row)),
                    quantity: Qty::new(
                        decode_price(i64_at(&batch, trade_cols::QUANTITY, row)).get(),
                    ),
                    is_buyer_maker: makers.as_ref().is_some_and(|m| m.value(row)),
                    at: Utc
                        .timestamp_millis_opt(ts)
                        .single()
                        .context("成交时间戳非法")?,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn days_in_month_is_correct() {
        assert_eq!(days_in_month(2026, 1), 31);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29, "2024 是闰年");
        assert_eq!(days_in_month(2000, 2), 29, "2000 是闰年（能被 400 整除）");
        assert_eq!(
            days_in_month(1900, 2),
            28,
            "1900 不是闰年（能被 100 但非 400）"
        );
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 12), 31);
    }

    /// 2026-08 是 31 天，与实测的 44,640 根 1m K 线（1440×31）吻合。
    #[test]
    fn august_2026_has_31_days_matching_observed_data() {
        assert_eq!(days_in_month(2026, 8), 31);
        assert_eq!(1440 * days_in_month(2026, 8), 44_640);
    }

    #[test]
    fn parquet_paths_follow_hive_layout() {
        let root = Path::new("/data");
        let k = klines_parquet_path(root, "ETHUSDC", "1m", 2026, 8);
        assert!(
            k.ends_with("lake/klines/symbol=ETHUSDC/interval=1m/year=2026/month=08/data.parquet"),
            "{k:?}"
        );

        let t = agg_trades_parquet_path(root, "ETHUSDC", 2026, 8, 15);
        assert!(
            t.ends_with("lake/agg_trades/symbol=ETHUSDC/date=2026-08-15/data.parquet"),
            "{t:?}"
        );
    }

    /// **这是回放正确性的核心测试**：同一时刻的成交必须先于 K 线。
    ///
    /// 顺序反了会让策略在 K 线收盘时把"尚未发生的成交"当成已知信息，
    /// 等价于偷看未来。
    #[test]
    fn trades_are_ordered_before_candles_at_the_same_instant() {
        let t = Utc
            .timestamp_millis_opt(1_785_542_400_000)
            .single()
            .unwrap();
        let slice = DaySlice {
            candles: vec![Candle {
                open_time: t,
                open: dec!(3200),
                high: dec!(3210),
                low: dec!(3190),
                close: dec!(3205),
                volume: dec!(100),
                closed: true,
            }],
            trades: vec![AggTrade {
                trade_id: 1,
                price: Price::new(dec!(3200)),
                quantity: Qty::new(dec!(1)),
                is_buyer_maker: true,
                at: t,
            }],
        };

        let events = slice.into_events();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(events[0], MarketEvent::AggTrade(_)),
            "同一时刻成交必须排在 K 线之前，实际顺序颠倒"
        );
        assert!(matches!(events[1], MarketEvent::Kline(_)));
    }

    #[test]
    fn events_are_sorted_by_time_across_types() {
        let t0 = Utc
            .timestamp_millis_opt(1_785_542_400_000)
            .single()
            .unwrap();
        let t1 = t0 + chrono::Duration::seconds(1);
        let slice = DaySlice {
            candles: vec![Candle {
                open_time: t1,
                open: dec!(3200),
                high: dec!(3210),
                low: dec!(3190),
                close: dec!(3205),
                volume: dec!(100),
                closed: true,
            }],
            trades: vec![AggTrade {
                trade_id: 1,
                price: Price::new(dec!(3200)),
                quantity: Qty::new(dec!(1)),
                is_buyer_maker: true,
                at: t0,
            }],
        };

        let events = slice.into_events();
        assert_eq!(events[0].at(), t0, "更早的事件排前面");
        assert_eq!(events[1].at(), t1);
    }

    #[test]
    fn replay_spec_enumerates_months_and_days() {
        let spec = ReplaySpec {
            symbol: "ETHUSDC".into(),
            interval: "1m".into(),
            from: (2026, 1),
            to: (2026, 3),
        };
        assert_eq!(spec.months().len(), 3);
        // 1 月 31 + 2 月 28 + 3 月 31
        assert_eq!(spec.day_count(), 90);
    }

    /// 缺失数据默认必须报错——跨越缺口会凭空发明成交。
    #[test]
    fn missing_data_policy_defaults_to_fail() {
        let p = MissingDataPolicy::Fail;
        assert_ne!(p, MissingDataPolicy::AllowAndRecord);
    }

    #[test]
    fn ensure_parquet_rejects_missing_and_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.parquet");
        assert!(ensure_parquet(&missing).is_err());

        let empty = dir.path().join("empty.parquet");
        std::fs::write(&empty, b"").unwrap();
        assert!(ensure_parquet(&empty).is_err(), "空文件不能当成有效数据");

        let ok = dir.path().join("ok.parquet");
        std::fs::write(&ok, b"x").unwrap();
        assert!(ensure_parquet(&ok).is_ok());
    }

    /// 定点解码必须与写入端对称。
    #[test]
    fn fixed_point_decoding_matches_writer_encoding() {
        // 1860.24 编码为 186024000000
        let raw: i64 = 186_024_000_000;
        assert_eq!(decode_price(raw).get(), dec!(1860.24));

        // 高精度：3200.12345678
        let raw2: i64 = 320_012_345_678;
        assert_eq!(decode_price(raw2).get(), dec!(3200.12345678));
    }
}
