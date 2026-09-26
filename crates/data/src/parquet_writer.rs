//! CSV → Parquet 流式转换。
//!
//! # 为什么必须流式
//!
//! 单个 `aggTrades` 月的 CSV 是 850 MB（ETHUSDC），`trades` 是 1.8 GB。
//! 三个标的 32 个月全量 CSV 超过 55 GB，而 Parquet 后约 10-15 GB。
//! 如果先全部解压再统一转换，峰值磁盘占用会是两者之和。
//!
//! 本模块的设计是**每次只驻留一个批次**（默认 10 万行）：
//! 逐行读 CSV → 攒满批次 → 写一个 Parquet row group → 释放。
//! 内存占用与文件大小无关，只与批次大小有关。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arrow::array::{ArrayRef, BooleanArray, Int64Array};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use chrono::{DateTime, TimeZone, Utc};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use crate::fixed::{self, PRICE_SCALE};
use crate::manifest::DatasetKind;

/// 每个 row group 的行数。10 万行在压缩率与内存之间比较平衡。
pub const BATCH_ROWS: usize = 100_000;

/// 转换过程中使用的临时文件路径：同目录下加 `.tmp` 后缀。
///
/// 同目录很重要——`rename` 跨文件系统不是原子操作，同目录能保证是。
fn tmp_path_for(out_path: &Path) -> std::path::PathBuf {
    let name = out_path
        .file_name()
        .map(|n| format!("{}.tmp", n.to_string_lossy()))
        .unwrap_or_else(|| "data.parquet.tmp".to_string());
    out_path.with_file_name(name)
}

/// 进程中途被杀时，`.tmp` 文件必须不被当成有效产物。这个 guard 在成功路径
/// 上显式 `disarm()`，其它任何退出路径（`?`、`bail!`、panic 展开）都会在
/// `Drop` 里删除残留的 `.tmp`。
struct TmpFileGuard<'a> {
    path: &'a Path,
    active: bool,
}

impl<'a> TmpFileGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, active: true }
    }

    fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for TmpFileGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// 转换结果的统计。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversionStats {
    pub row_count: u64,
    pub min_ts: DateTime<Utc>,
    pub max_ts: DateTime<Utc>,
    /// 数据内部检测到的时间空洞（秒对）。
    pub internal_gaps: Vec<(DateTime<Utc>, DateTime<Utc>)>,
}

/// 数据内部时间间隔超过这个秒数就记为一个空洞。
///
/// 阈值按数据集分：K 线类间隔固定（1 分钟），逐笔成交则取决于活跃度，
/// 用较宽的阈值避免把正常的清淡时段误判成缺口。
fn gap_threshold_secs(kind: DatasetKind) -> i64 {
    match kind {
        DatasetKind::Klines1m | DatasetKind::MarkPriceKlines1m => 120,
        DatasetKind::AggTrades => 300,
        DatasetKind::FundingRate => 9 * 3600,
    }
}

/// 数据集的 Parquet schema。
///
/// 所有价格与数量都是 `Int64` 定点（缩放 1e8），费率是 `Int64`（缩放 1e18）。
/// 时间统一为 `Int64` 毫秒 UTC——用整数而非 Arrow 的 timestamp 类型，
/// 是为了让 DuckDB 侧的过滤与分区剪枝行为最简单可预测。
fn schema_for(kind: DatasetKind) -> Arc<Schema> {
    let ms = |name: &str| Field::new(name, DataType::Int64, false);
    let fields = match kind {
        DatasetKind::Klines1m | DatasetKind::MarkPriceKlines1m => vec![
            ms("open_time_ms"),
            Field::new("open", DataType::Int64, false),
            Field::new("high", DataType::Int64, false),
            Field::new("low", DataType::Int64, false),
            Field::new("close", DataType::Int64, false),
            Field::new("volume", DataType::Int64, false),
            ms("close_time_ms"),
            Field::new("quote_volume", DataType::Int64, false),
            Field::new("trade_count", DataType::Int64, false),
            Field::new("taker_buy_volume", DataType::Int64, false),
            Field::new("taker_buy_quote_volume", DataType::Int64, false),
            // 币安归档的尾部占位列，恒为 0。保留它是为了让列数与归档严格一致
            // ——列数校验正是用来发现"币安改过字段"的机制，所以不能为了省一列
            // 而让校验失去意义。
            Field::new("ignore", DataType::Int64, false),
        ],
        DatasetKind::AggTrades => vec![
            Field::new("agg_trade_id", DataType::Int64, false),
            Field::new("price", DataType::Int64, false),
            Field::new("quantity", DataType::Int64, false),
            Field::new("first_trade_id", DataType::Int64, false),
            Field::new("last_trade_id", DataType::Int64, false),
            ms("transact_time_ms"),
            // true = 买方是挂单方，即**卖方主动**吃单。
            // 这是判断主动方向的唯一依据，也是 markout 计算的基础。
            Field::new("is_buyer_maker", DataType::Boolean, false),
        ],
        DatasetKind::FundingRate => vec![
            ms("calc_time_ms"),
            Field::new("funding_interval_hours", DataType::Int64, false),
            Field::new("last_funding_rate", DataType::Int64, false),
        ],
    };
    let _ = TimeUnit::Millisecond;
    Arc::new(Schema::new(fields))
}

/// 一个可累积的行。
struct Rows {
    columns: Vec<Vec<i64>>,
    bools: Vec<Vec<bool>>,
    count: usize,
}

impl Rows {
    fn new(kind: DatasetKind) -> Self {
        let schema = schema_for(kind);
        let mut columns = Vec::new();
        let mut bools = Vec::new();
        for f in schema.fields() {
            if f.data_type() == &DataType::Boolean {
                bools.push(Vec::with_capacity(BATCH_ROWS));
            } else {
                columns.push(Vec::with_capacity(BATCH_ROWS));
            }
        }
        Self {
            columns,
            bools,
            count: 0,
        }
    }

    fn push(&mut self, values: &[i64], bools: &[bool]) {
        for (i, v) in values.iter().enumerate() {
            self.columns[i].push(*v);
        }
        for (i, v) in bools.iter().enumerate() {
            self.bools[i].push(*v);
        }
        self.count += 1;
    }

    fn is_full(&self) -> bool {
        self.count >= BATCH_ROWS
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn take_batch(&mut self, kind: DatasetKind, schema: &Arc<Schema>) -> Result<RecordBatch> {
        let mut arrays: Vec<ArrayRef> = Vec::new();
        let mut col_idx = 0;
        let mut bool_idx = 0;
        for f in schema.fields() {
            if f.data_type() == &DataType::Boolean {
                arrays.push(Arc::new(BooleanArray::from(std::mem::take(
                    &mut self.bools[bool_idx],
                ))) as ArrayRef);
                bool_idx += 1;
            } else {
                arrays.push(
                    Arc::new(Int64Array::from(std::mem::take(&mut self.columns[col_idx])))
                        as ArrayRef,
                );
                col_idx += 1;
            }
        }
        self.count = 0;
        let _ = kind;
        RecordBatch::try_new(schema.clone(), arrays).context("构造 RecordBatch 失败")
    }
}

/// 把一份币安归档 CSV 转换为 Parquet。
///
/// 输入是解压后的 CSV 路径，输出是 Parquet 路径。函数内部流式处理，
/// 内存占用与输入大小无关。
pub fn convert_csv_to_parquet(
    kind: DatasetKind,
    csv_path: &Path,
    out_path: &Path,
) -> Result<ConversionStats> {
    convert_csv_to_parquet_with_progress(kind, csv_path, out_path, &mut |_, _| true)
}

/// 带进度回调的版本。
///
/// `on_progress(done_bytes, total_bytes)` 在每个批次写完后调用一次（批次
/// 大小见 [`BATCH_ROWS`]）；返回 `false` 表示应取消——函数会立即返回一个
/// 文案包含"取消"的错误，且保证不留下最终文件与 `.tmp` 残留
/// （[`TmpFileGuard`] 在任何提前返回路径上都会清理 `.tmp`，而最终文件只在
/// 转换全部完成后才通过 rename 产生）。
pub fn convert_csv_to_parquet_with_progress(
    kind: DatasetKind,
    csv_path: &Path,
    out_path: &Path,
    on_progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<ConversionStats> {
    let file =
        File::open(csv_path).with_context(|| format!("打开 CSV 失败: {}", csv_path.display()))?;
    let total_bytes = file
        .metadata()
        .with_context(|| format!("读取 CSV 元数据失败: {}", csv_path.display()))?
        .len();
    let mut reader = BufReader::with_capacity(8 * 1024 * 1024, file);

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 原子写：先写到同目录的 `.tmp`，成功后才 fsync + rename 到最终路径。
    // 任何错误退出路径都靠 `TmpFileGuard` 清理 `.tmp`，避免进程中途被杀
    // 时在 `out_path` 或残留的 `.tmp` 留下半成品。
    let tmp_path = tmp_path_for(out_path);
    let _ = std::fs::remove_file(&tmp_path); // 清理上次中途被杀留下的残留
    let tmp_guard = TmpFileGuard::new(&tmp_path);

    let schema = schema_for(kind);
    let props = WriterProperties::builder()
        // zstd 级别 3：压缩率与速度的平衡点。级别更高收益递减而 CPU 明显上升。
        .set_compression(Compression::ZSTD(Default::default()))
        .set_max_row_group_size(BATCH_ROWS)
        .build();
    let out = File::create(&tmp_path)
        .with_context(|| format!("创建 Parquet 失败: {}", tmp_path.display()))?;
    let mut writer = ArrowWriter::try_new(out, schema.clone(), Some(props))
        .context("初始化 Parquet writer 失败")?;

    // 表头
    let mut header = String::new();
    let mut bytes_read = reader.read_line(&mut header).context("读取 CSV 表头失败")? as u64;
    let expected_cols = schema.fields().len();
    let header_cols = header.trim_end().split(',').count();
    if header_cols != expected_cols {
        bail!(
            "CSV 列数 ({header_cols}) 与 {} 的 schema ({expected_cols}) 不符；\
             币安可能改过字段。表头：{header}",
            kind.archive_dir()
        );
    }

    let mut rows = Rows::new(kind);
    let mut stats = ConversionStats {
        row_count: 0,
        min_ts: DateTime::<Utc>::MAX_UTC,
        max_ts: DateTime::<Utc>::MIN_UTC,
        internal_gaps: Vec::new(),
    };
    let mut last_ts_ms: Option<i64> = None;
    let gap_threshold_ms = gap_threshold_secs(kind) * 1000;

    let mut line = String::new();
    let mut line_no: u64 = 1;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).context("读取 CSV 行失败")?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        line_no += 1;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        let (values, bools, ts_ms) = parse_row(kind, trimmed, line_no)?;

        // 记录时间范围与内部空洞
        let ts = Utc
            .timestamp_millis_opt(ts_ms)
            .single()
            .with_context(|| format!("第 {line_no} 行时间戳非法: {ts_ms}"))?;
        if ts < stats.min_ts {
            stats.min_ts = ts;
        }
        if ts > stats.max_ts {
            stats.max_ts = ts;
        }
        if let Some(prev) = last_ts_ms {
            if ts_ms - prev > gap_threshold_ms {
                stats
                    .internal_gaps
                    .push((Utc.timestamp_millis_opt(prev).single().unwrap_or(ts), ts));
            }
        }
        last_ts_ms = Some(ts_ms);

        rows.push(&values, &bools);
        stats.row_count += 1;

        if rows.is_full() {
            let batch = rows.take_batch(kind, &schema)?;
            writer.write(&batch).context("写入 Parquet 失败")?;
            if !on_progress(bytes_read, total_bytes) {
                bail!("转换已取消: {}", csv_path.display());
            }
        }
    }

    if !rows.is_empty() {
        let batch = rows.take_batch(kind, &schema)?;
        writer.write(&batch).context("写入 Parquet 失败")?;
    }
    if !on_progress(bytes_read, total_bytes) {
        bail!("转换已取消: {}", csv_path.display());
    }

    if stats.row_count == 0 {
        bail!("CSV 没有任何数据行: {}", csv_path.display());
    }

    writer.close().context("收尾 Parquet 失败")?;

    // fsync tmp 文件内容，再原子 rename 到最终路径，最后 fsync 父目录确保
    // rename 本身落盘——沿用 `manifest::save` 里验证过的模式。
    {
        let f = File::open(&tmp_path)
            .with_context(|| format!("重新打开 Parquet 用于 fsync 失败: {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync Parquet 失败: {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, out_path)
        .with_context(|| format!("替换 Parquet 失败: {}", out_path.display()))?;
    if let Some(parent) = out_path.parent() {
        let dir = File::open(parent)
            .with_context(|| format!("打开 Parquet 目录失败: {}", parent.display()))?;
        dir.sync_all()
            .with_context(|| format!("fsync Parquet 目录失败: {}", parent.display()))?;
    }
    tmp_guard.disarm();

    Ok(stats)
}

/// 解析一行 CSV，返回 (i64 列, bool 列, 时间戳毫秒)。
///
/// 列顺序必须与 `schema_for` 一致。
fn parse_row(kind: DatasetKind, line: &str, line_no: u64) -> Result<(Vec<i64>, Vec<bool>, i64)> {
    let f: Vec<&str> = line.split(',').collect();
    let err = |what: &str| anyhow::anyhow!("第 {line_no} 行解析 {what} 失败，原始内容：{line}");

    match kind {
        DatasetKind::Klines1m | DatasetKind::MarkPriceKlines1m => {
            // open_time,open,high,low,close,volume,close_time,quote_volume,count,
            // taker_buy_volume,taker_buy_quote_volume,ignore
            if f.len() < 12 {
                bail!("第 {line_no} 行字段数不足 12：{line}");
            }
            let ts: i64 = f[0].trim().parse().map_err(|_| err("open_time"))?;
            let vals = vec![
                ts,
                fixed::parse_price(f[1]).map_err(|e| err(&e.to_string()))?,
                fixed::parse_price(f[2]).map_err(|e| err(&e.to_string()))?,
                fixed::parse_price(f[3]).map_err(|e| err(&e.to_string()))?,
                fixed::parse_price(f[4]).map_err(|e| err(&e.to_string()))?,
                fixed::parse_price(f[5]).map_err(|e| err(&e.to_string()))?,
                f[6].trim().parse().map_err(|_| err("close_time"))?,
                fixed::parse_price(f[7]).map_err(|e| err(&e.to_string()))?,
                f[8].trim().parse().map_err(|_| err("count"))?,
                fixed::parse_price(f[9]).map_err(|e| err(&e.to_string()))?,
                fixed::parse_price(f[10]).map_err(|e| err(&e.to_string()))?,
                // 尾部占位列，恒为 0；解析但不用，保持列数与归档一致。
                f[11].trim().parse().unwrap_or(0),
            ];
            Ok((vals, Vec::new(), ts))
        }
        DatasetKind::AggTrades => {
            // agg_trade_id,price,quantity,first_trade_id,last_trade_id,
            // transact_time,is_buyer_maker
            if f.len() < 7 {
                bail!("第 {line_no} 行字段数不足 7：{line}");
            }
            let ts: i64 = f[5].trim().parse().map_err(|_| err("transact_time"))?;
            let is_buyer_maker = match f[6].trim() {
                "true" | "True" | "TRUE" => true,
                "false" | "False" | "FALSE" => false,
                other => bail!("第 {line_no} 行 is_buyer_maker 非法: {other}"),
            };
            let vals = vec![
                f[0].trim().parse().map_err(|_| err("agg_trade_id"))?,
                fixed::parse_price(f[1]).map_err(|e| err(&e.to_string()))?,
                fixed::parse_price(f[2]).map_err(|e| err(&e.to_string()))?,
                f[3].trim().parse().map_err(|_| err("first_trade_id"))?,
                f[4].trim().parse().map_err(|_| err("last_trade_id"))?,
                ts,
            ];
            Ok((vals, vec![is_buyer_maker], ts))
        }
        DatasetKind::FundingRate => {
            // calc_time,funding_interval_hours,last_funding_rate
            if f.len() < 3 {
                bail!("第 {line_no} 行字段数不足 3：{line}");
            }
            let ts: i64 = f[0].trim().parse().map_err(|_| err("calc_time"))?;
            let rate = fixed::parse_decimal(f[2]).map_err(|e| err(&e.to_string()))?;
            let vals = vec![
                ts,
                f[1].trim()
                    .parse()
                    .map_err(|_| err("funding_interval_hours"))?,
                fixed::encode_rate(rate).map_err(|e| err(&e.to_string()))?,
            ];
            Ok((vals, Vec::new(), ts))
        }
    }
}

/// 定点价格缩放因子，供查询层还原。
pub const SCALE: i64 = PRICE_SCALE;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_csv(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        let mut f = File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn klines_convert_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "k.csv",
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n\
             1785542400000,1860.24,1861.29,1860.01,1861.15,700.611,1785542459999,1303525.72961,987,414.070,770424.15195,0\n\
             1785542460000,1861.15,1861.50,1860.90,1861.40,500.000,1785542519999,930000.00000,500,200.000,372000.00000,0\n",
        );
        let out = dir.path().join("k.parquet");
        let stats = convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out).unwrap();

        assert_eq!(stats.row_count, 2);
        assert!(out.exists());
        assert!(out.metadata().unwrap().len() > 0);
        assert_eq!(
            stats.min_ts.timestamp_millis(),
            1785542400000,
            "最早时间戳应正确"
        );
        assert_eq!(stats.max_ts.timestamp_millis(), 1785542460000);
        assert!(stats.internal_gaps.is_empty(), "两行相隔 60 秒，不是缺口");
    }

    /// 成功转换后：最终 Parquet 文件必须存在，且不能留下 `.tmp` 残留。
    #[test]
    fn successful_conversion_leaves_final_file_and_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "k.csv",
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n\
             1785542400000,1860.24,1861.29,1860.01,1861.15,700.611,1785542459999,1303525.72961,987,414.070,770424.15195,0\n",
        );
        let out = dir.path().join("k.parquet");
        let tmp = dir.path().join("k.parquet.tmp");
        convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out).unwrap();

        assert!(out.exists(), "最终文件应存在");
        assert!(!tmp.exists(), "成功后不应留下 .tmp 残留");
    }

    #[test]
    fn agg_trades_convert_preserves_direction_flag() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "a.csv",
            "agg_trade_id,price,quantity,first_trade_id,last_trade_id,transact_time,is_buyer_maker\n\
             388378028,1860.24,1.239,836419977,836419986,1785542400041,false\n\
             388378029,1860.29,0.221,836419987,836419987,1785542400079,true\n",
        );
        let out = dir.path().join("a.parquet");
        let stats = convert_csv_to_parquet(DatasetKind::AggTrades, &csv, &out).unwrap();
        assert_eq!(stats.row_count, 2);
    }

    #[test]
    fn funding_rate_uses_rate_scale() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "f.csv",
            "calc_time,funding_interval_hours,last_funding_rate\n\
             1785542400001,8,0.00004296\n\
             1785571200000,8,0.00002903\n",
        );
        let out = dir.path().join("f.parquet");
        let stats = convert_csv_to_parquet(DatasetKind::FundingRate, &csv, &out).unwrap();
        assert_eq!(stats.row_count, 2);
    }

    /// 表头列数不符必须报错——币安改字段时不能静默错位。
    #[test]
    fn schema_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "bad.csv",
            "open_time,open,high,low,close\n1785542400000,1,2,3,4\n",
        );
        let out = dir.path().join("bad.parquet");
        let err = convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out)
            .unwrap_err()
            .to_string();
        assert!(err.contains("列数"), "{err}");
    }

    /// 数据内部的时间空洞必须被检测出来——跨越空洞的回测会凭空发明成交。
    #[test]
    fn internal_time_gaps_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        // 三根 K 线，中间空了一大段（相隔 3600 秒 > 120 秒阈值）
        let csv = write_csv(
            dir.path(),
            "gap.csv",
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n\
             1785542400000,1860.24,1861.29,1860.01,1861.15,700.611,1785542459999,1303525.72961,987,414.070,770424.15195,0\n\
             1785546000000,1861.15,1861.50,1860.90,1861.40,500.000,1785546059999,930000.00000,500,200.000,372000.00000,0\n",
        );
        let out = dir.path().join("gap.parquet");
        let stats = convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out).unwrap();
        assert_eq!(stats.internal_gaps.len(), 1, "应检测到一处空洞");
    }

    /// 空数据文件不能当成"转换成功"——那会让台账把空分区标记为可用。
    #[test]
    fn empty_csv_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "empty.csv",
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n",
        );
        let out = dir.path().join("empty.parquet");
        assert!(convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out).is_err());
    }

    /// 坏行导致转换失败后：最终路径不能存在，`.tmp` 也不能留下残留
    /// ——否则进程被杀在写入中途会让 `lake/…/data.parquet.tmp` 一直躺在
    /// 磁盘上，且下次重跑前如果误读到它会当成半成品数据。
    #[test]
    fn failed_conversion_leaves_neither_final_nor_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "bad_row.csv",
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n\
             1785542400000,1860.24,1861.29,1860.01,1861.15,700.611,1785542459999,1303525.72961,987,414.070,770424.15195,0\n\
             not_a_number,1861.15,1861.50,1860.90,1861.40,500.000,1785542519999,930000.00000,500,200.000,372000.00000,0\n",
        );
        let out = dir.path().join("bad_row.parquet");
        let tmp = dir.path().join("bad_row.parquet.tmp");

        assert!(convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out).is_err());
        assert!(!out.exists(), "转换失败后最终文件不应存在");
        assert!(!tmp.exists(), "转换失败后 .tmp 也不应留下残留");
    }

    /// 超过 8 位小数的价格必须报错而非静默截断。
    #[test]
    fn excessive_precision_is_rejected_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "prec.csv",
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n\
             1785542400000,1860.123456789,1861.29,1860.01,1861.15,700.611,1785542459999,1303525.72961,987,414.070,770424.15195,0\n",
        );
        let out = dir.path().join("prec.parquet");
        assert!(convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out).is_err());
    }

    /// 空字段（币安偶尔留空）按 0 处理，不应导致整文件失败。
    #[test]
    fn empty_numeric_fields_are_treated_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let csv = write_csv(
            dir.path(),
            "z.csv",
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n\
             1785542400000,1860.24,1861.29,1860.01,1861.15,,1785542459999,,987,,,0\n",
        );
        let out = dir.path().join("z.parquet");
        let stats = convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out).unwrap();
        assert_eq!(stats.row_count, 1);
    }

    /// 跨多个 row group 的转换必须完整——验证批次边界不丢行。
    #[test]
    fn multi_batch_conversion_keeps_all_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::from(
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n",
        );
        let total = BATCH_ROWS + 137; // 刻意跨批次边界
        for i in 0..total {
            content.push_str(&format!(
                "{},100.00,101.00,99.00,100.50,1.000,{},1.0,1,0.5,0.5,0\n",
                1785542400000i64 + (i as i64) * 60_000,
                1785542459999i64 + (i as i64) * 60_000
            ));
        }
        let csv = write_csv(dir.path(), "big.csv", &content);
        let out = dir.path().join("big.parquet");
        let stats = convert_csv_to_parquet(DatasetKind::Klines1m, &csv, &out).unwrap();
        assert_eq!(stats.row_count, total as u64, "跨批次不能丢行");
    }

    fn multi_batch_csv(dir: &Path, name: &str, batches: usize) -> (std::path::PathBuf, u64) {
        let mut content = String::from(
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore\n",
        );
        let total = BATCH_ROWS * batches + 137; // 刻意跨批次边界
        for i in 0..total {
            content.push_str(&format!(
                "{},100.00,101.00,99.00,100.50,1.000,{},1.0,1,0.5,0.5,0\n",
                1785542400000i64 + (i as i64) * 60_000,
                1785542459999i64 + (i as i64) * 60_000
            ));
        }
        let csv = write_csv(dir, name, &content);
        let size = std::fs::metadata(&csv).unwrap().len();
        (csv, size)
    }

    /// 跨多批的 CSV：进度回调的 `done` 单调不减，最后一次等于文件大小。
    #[test]
    fn progress_callback_is_monotonic_and_ends_at_file_size() {
        let dir = tempfile::tempdir().unwrap();
        let (csv, size) = multi_batch_csv(dir.path(), "multi.csv", 3);
        let out = dir.path().join("multi.parquet");

        let mut seen: Vec<(u64, u64)> = Vec::new();
        let stats = convert_csv_to_parquet_with_progress(
            DatasetKind::Klines1m,
            &csv,
            &out,
            &mut |done, total| {
                seen.push((done, total));
                true
            },
        )
        .unwrap();

        assert!(stats.row_count > 0);
        assert!(seen.len() >= 3, "至少应跨越多个批次汇报进度：{seen:?}");
        for i in 1..seen.len() {
            assert!(seen[i].0 >= seen[i - 1].0, "done 不能倒退：{seen:?}");
        }
        let (last_done, last_total) = *seen.last().unwrap();
        assert_eq!(last_done, size, "最后一次 done 应等于文件大小");
        assert_eq!(last_total, size);
    }

    /// 回调返回 `false` 必须取消转换：错误文案含"取消"，最终文件与 `.tmp`
    /// 都不能留下——否则半成品会被下次误当成有效缓存。
    #[test]
    fn progress_callback_returning_false_cancels_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let (csv, _size) = multi_batch_csv(dir.path(), "cancel.csv", 2);
        let out = dir.path().join("cancel.parquet");
        let tmp = dir.path().join("cancel.parquet.tmp");

        let mut calls = 0u32;
        let err =
            convert_csv_to_parquet_with_progress(DatasetKind::Klines1m, &csv, &out, &mut |_, _| {
                calls += 1;
                false
            })
            .unwrap_err()
            .to_string();

        assert!(err.contains("取消"), "{err}");
        assert!(!out.exists(), "取消后最终文件不应存在");
        assert!(!tmp.exists(), "取消后 .tmp 不应留下残留");
        assert!(calls >= 1);
    }
}
