//! 转换单个已解压的币安 CSV，用于人工核对与调试。
//!
//! 用法：
//! ```sh
//! cargo run -p data --example convert_one -- agg_trades /path/in.csv /path/out.parquet
//! ```
//!
//! 注意：本文件的体积与耗时展示**只用整数运算**。`f32`/`f64` 在编译期被
//! `clippy.toml` 禁用（交易数值必须用 `Decimal`），这里虽然只是展示用途，
//! 但保持全局无浮点能让禁用规则保持无豁免地生效——规则一旦开了口子就会
//! 逐渐被绕过。

use std::path::PathBuf;
use std::time::Duration;

use data::manifest::DatasetKind;
use data::parquet_writer::convert_csv_to_parquet;

/// 把字节数格式化为 MB，保留一位小数，纯整数运算。
fn mb(bytes: u64) -> String {
    let tenths = (bytes * 10) / 1_000_000;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// 压缩比，保留两位小数，纯整数运算。
fn ratio(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        return "n/a".into();
    }
    let hundredths = (numerator * 100) / denominator;
    format!("{}.{}", hundredths / 100, hundredths % 100)
}

fn secs(d: Duration) -> String {
    let tenths = d.as_millis() / 100;
    format!("{}.{}", tenths / 10, tenths % 10)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let kind_arg = args.next().unwrap_or_else(|| {
        eprintln!(
            "用法: convert_one <klines|agg_trades|mark_price|funding> <in.csv> <out.parquet>"
        );
        std::process::exit(2);
    });
    let kind = match kind_arg.as_str() {
        "klines" => DatasetKind::Klines1m,
        "agg_trades" => DatasetKind::AggTrades,
        "mark_price" => DatasetKind::MarkPriceKlines1m,
        "funding" => DatasetKind::FundingRate,
        other => {
            eprintln!("未知数据集类型: {other}");
            std::process::exit(2);
        }
    };
    let input = PathBuf::from(args.next().expect("缺少输入 CSV 路径"));
    let output = PathBuf::from(args.next().expect("缺少输出 Parquet 路径"));

    let started = std::time::Instant::now();
    let stats = convert_csv_to_parquet(kind, &input, &output)?;
    let elapsed = started.elapsed();

    let in_bytes = std::fs::metadata(&input)?.len();
    let out_bytes = std::fs::metadata(&output)?.len();

    println!("数据集       {kind:?}");
    println!("输入         {} ({} MB)", input.display(), mb(in_bytes));
    println!("输出         {} ({} MB)", output.display(), mb(out_bytes));
    println!("压缩比       {}x", ratio(in_bytes, out_bytes));
    println!("行数         {}", stats.row_count);
    println!("时间范围     {} .. {}", stats.min_ts, stats.max_ts);
    println!("内部空洞     {} 处", stats.internal_gaps.len());
    for (from, to) in stats.internal_gaps.iter().take(5) {
        println!("  {from} .. {to}");
    }
    println!("耗时         {}s", secs(elapsed));
    Ok(())
}
