//! 命令行入口。
//!
//! 子命令：
//! - `download` —— 下载并转换历史数据到本地
//! - `backtest` —— 用本地数据跑回测并打印对比表
//! - `coverage` —— 查看本地数据覆盖与缺口
//! - `strategies` —— 列出可用策略与参数说明
//! - `models` —— 列出可用的成交模型
//!
//! 设计取向：**每个子命令都输出人能直接读的表格**，而不只是 JSON。
//! 因为回测结论的可信度需要人来判断——比如"乐观模型盈利、诚实模型亏损"
//! 这种符号翻转，必须是显眼的一行字，而不是埋在 JSON 里等前端渲染。

mod backtest;
mod download;
mod format;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// 默认数据根目录。
const DEFAULT_DATA_ROOT: &str = "data";

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(|s| s.as_str()) else {
        print_usage();
        return Ok(());
    };

    match cmd {
        "download" => download::run(&args[1..]).await,
        "backtest" => backtest::run(&args[1..]).await,
        "coverage" => coverage(&args[1..]),
        "strategies" => list_strategies(),
        "models" => list_models(),
        "help" | "-h" | "--help" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("未知子命令：{other}\n");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("cli=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn print_usage() {
    println!(
        r#"rust-crypto 命令行

用法：
  rc download   [选项]    下载并转换历史数据
  rc backtest   [选项]    跑回测并打印对比表
  rc coverage   [选项]    查看本地数据覆盖与缺口
  rc strategies           列出可用策略与参数说明
  rc models               列出可用的成交模型

download 选项：
  --symbol <SYM>          交易对（可重复，或用逗号分隔）
  --kind <KIND>           数据集：klines / agg_trades / mark_price / funding
                          （可重复，或用逗号分隔）
  --from <YYYY-MM|earliest>  起始月份，或 earliest（归档实际最早月份）
  --to <YYYY-MM|latest>      结束月份，或 latest（归档实际最晚月份）
  --data-root <PATH>      数据根目录（默认 {DEFAULT_DATA_ROOT}）
  --concurrency <N>       并发下载数（默认 4）

backtest 选项：
  --symbol <SYM>          交易对
  --strategy <ID>         策略 ID（默认 range_maker）
  --from <YYYY-MM-DD>     起始日期
  --to <YYYY-MM-DD>       结束日期
  --fill-models <LIST>    成交模型，逗号分隔（默认 m0,m1）
  --equity <DECIMAL>      初始权益（默认 10000）
  --data-root <PATH>      数据根目录

coverage 选项：
  --symbol <SYM>          只显示该交易对
  --data-root <PATH>      数据根目录

示例：
  rc download --symbol ETHUSDC --kind klines,agg_trades --from 2026-01 --to 2026-08
  rc download --symbol ETHUSDC --kind klines,agg_trades --from earliest --to latest
  rc backtest --symbol ETHUSDC --from 2026-08-01 --to 2026-08-31 --fill-models m0,m1
"#
    );
}

/// 解析 `--key value` 形式的参数。返回该 key 的所有出现值。
pub fn flag_values(args: &[String], key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == key && i + 1 < args.len() {
            out.push(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// 取单个值，后者覆盖前者。
pub fn flag_one(args: &[String], key: &str) -> Option<String> {
    flag_values(args, key).pop()
}

/// 取单个值，带默认。
pub fn flag_or(args: &[String], key: &str, default: &str) -> String {
    flag_one(args, key).unwrap_or_else(|| default.to_string())
}

/// 解析逗号分隔并可重复的列表参数。
///
/// 支持 `--kind a,b` 与 `--kind a --kind b` 两种写法——前者更简洁，
/// 后者便于脚本拼接。
pub fn flag_list(args: &[String], key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for v in flag_values(args, key) {
        for part in v.split(',') {
            let t = part.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}

pub fn flag_parse<T: std::str::FromStr>(args: &[String], key: &str) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    match flag_one(args, key) {
        None => Ok(None),
        Some(v) => v
            .parse::<T>()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("参数 {key} 的值 {v} 非法：{e}")),
    }
}

pub fn data_root(args: &[String]) -> PathBuf {
    PathBuf::from(flag_or(args, "--data-root", DEFAULT_DATA_ROOT))
}

/// 解析 `YYYY-MM` 月份。
pub fn parse_month(s: &str) -> Result<(i32, u32)> {
    let (y, m) = s
        .split_once('-')
        .with_context(|| format!("月份格式应为 YYYY-MM，收到：{s}"))?;
    let year: i32 = y.parse().with_context(|| format!("非法年份：{y}"))?;
    let month: u32 = m.parse().with_context(|| format!("非法月份：{m}"))?;
    if !(1..=12).contains(&month) {
        bail!("月份必须在 1-12 之间，收到：{month}");
    }
    Ok((year, month))
}

/// 解析 `YYYY-MM-DD` 日期。
pub fn parse_date(s: &str) -> Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("日期格式应为 YYYY-MM-DD，收到：{s}"))
}

/// 查看本地数据覆盖与缺口。
fn coverage(args: &[String]) -> Result<()> {
    use data::{Layout, Manifest};

    let root = data_root(args);
    let layout = Layout::new(&root);
    let manifest = Manifest::load(&layout.manifest_path())?;

    let filter = flag_one(args, "--symbol");
    println!("数据根目录：{}\n", root.display());

    if manifest.partitions.is_empty() {
        println!("台账为空——尚未下载任何数据。");
        println!(
            "\n先跑：rc download --symbol ETHUSDC --kind klines,agg_trades --from 2026-01 --to 2026-08"
        );
        return Ok(());
    }

    // 按 (数据集, 交易对) 分组统计
    // (数据集, 交易对) -> 各月份的状态
    type MonthStatus = (i32, u32, String, u64);
    let mut groups: std::collections::BTreeMap<(String, String), Vec<MonthStatus>> =
        Default::default();
    for (key, entry) in &manifest.partitions {
        if let Some(s) = &filter {
            if &key.symbol != s {
                continue;
            }
        }
        let status = match &entry.status {
            data::PartitionStatus::Finalized { .. } => "完成".to_string(),
            data::PartitionStatus::Suspicious {
                row_count,
                expected,
                ..
            } => format!("待查（{row_count} 行，期望 {expected}）"),
            data::PartitionStatus::NotInArchive => "归档无此分区".to_string(),
            data::PartitionStatus::Failed { error, .. } => format!("失败：{error}"),
            data::PartitionStatus::Absent => "未处理".to_string(),
        };
        groups
            .entry((format!("{:?}", key.kind), key.symbol.clone()))
            .or_default()
            .push((
                key.year,
                key.month,
                status,
                entry.parquet_bytes.unwrap_or(0),
            ));
    }

    if groups.is_empty() {
        println!("没有匹配的数据。");
        return Ok(());
    }

    for ((kind, symbol), mut months) in groups {
        months.sort();
        let total_bytes: u64 = months.iter().map(|(_, _, _, b)| *b).sum();
        let done = months.iter().filter(|(_, _, s, _)| s == "完成").count();
        println!("{symbol}  {kind}");
        format::kv("分区数", &format!("{} 个（完成 {done}）", months.len()));
        format::kv("落盘大小", &format!("{} MB", format::mb(total_bytes)));
        let first = months
            .first()
            .map(|(y, m, _, _)| format!("{y}-{m:02}"))
            .unwrap_or_default();
        let last = months
            .last()
            .map(|(y, m, _, _)| format!("{y}-{m:02}"))
            .unwrap_or_default();
        format::kv("覆盖区间", &format!("{first} .. {last}"));

        let problems: Vec<_> = months
            .iter()
            .filter(|(_, _, s, _)| s != "完成")
            .map(|(y, m, s, _)| format!("{y}-{m:02}: {s}"))
            .collect();
        if !problems.is_empty() {
            format::kv("异常分区", &format!("{} 个", problems.len()));
            for p in problems.iter().take(10) {
                println!("    {p}");
            }
            if problems.len() > 10 {
                println!("    ...以及另外 {} 个", problems.len() - 10);
            }
        }
        println!();
    }

    if !manifest.gaps.is_empty() {
        println!("记录在案的缺口：{} 处", manifest.gaps.len());
        for g in manifest.gaps.iter().take(10) {
            println!(
                "  {:?} {} {} .. {}  {}",
                g.kind, g.symbol, g.from, g.to, g.note
            );
        }
        println!("\n注意：跨越缺口的回测会凭空发明成交，默认会被拒绝运行。");
    }

    Ok(())
}

fn list_strategies() -> Result<()> {
    println!("可用策略：\n");
    for s in strategies::catalog() {
        println!("  {} — {}", s.id, s.name);
        println!("    决策前需要 {} 根已收盘 K 线", s.warmup_candles);
        println!("    参数：");

        // 按显示宽度对齐参数名列（中文按 2 列宽）。
        let key_width = s
            .parameters
            .iter()
            .map(|p| p.key.chars().count())
            .max()
            .unwrap_or(8);

        for p in &s.parameters {
            let unit = p.unit.as_deref().unwrap_or("");
            println!("      {}  {}", pad_display(&p.key, key_width), p.label);
            println!(
                "        默认 {}   范围 {} ~ {}",
                display_value(p.default, unit),
                display_value(p.min, unit),
                display_value(p.max, unit)
            );
            for line in wrap(&p.description, 64) {
                println!("        {line}");
            }
            println!();
        }
    }
    Ok(())
}

/// 按显示宽度补齐（中文按 2 列宽）。
fn pad_display(s: &str, width: usize) -> String {
    let w: usize = s
        .chars()
        .map(|c| if (c as u32) > 0x2E80 { 2 } else { 1 })
        .sum();
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

/// 参数值的展示形式。
///
/// 关键：`%` 单位的参数存的是**比例**（0.1 = 10%），所以展示时要乘 100。
/// 直接打印 `0.1%` 会让人以为仓位只有千分之一，与 10% 相差 100 倍。
fn display_value(v: rust_decimal::Decimal, unit: &str) -> String {
    if unit == "%" {
        format!("{}%", (v * rust_decimal::Decimal::from(100)).normalize())
    } else {
        let n = v.normalize().to_string();
        if unit.is_empty() {
            n
        } else {
            format!("{n}{unit}")
        }
    }
}

fn list_models() -> Result<()> {
    println!("可用成交模型：\n");
    for name in sim::MODEL_NAMES {
        if let Some(m) = sim::model_by_name(name) {
            let optimism = match m.optimism() {
                sim::Optimism::UpperBound => "上界（不现实，仅供对照）",
                sim::Optimism::ConservativeLower => "保守下界（诚实基线）",
            };
            println!("  {name:<4} {}", m.name());
            println!("    数据需求    {}", m.data_requirements());
            println!("    乐观程度    {optimism}");
            println!();
        }
    }
    println!("回测默认同时跑 m0 与 m1 并报告两者差异。");
    println!("若两者结论方向相反，结果会被标记为不可信——这是刻意的设计。");
    Ok(())
}

/// 按字符宽度折行（中文按 2 列宽计）。
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = if (ch as u32) > 0x2E80 { 2 } else { 1 };
        if w + cw > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(ch);
        w += cw;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn flag_values_collects_repeated_occurrences() {
        let a = args(&["--symbol", "ETHUSDC", "--symbol", "BTCUSDC"]);
        assert_eq!(flag_values(&a, "--symbol"), vec!["ETHUSDC", "BTCUSDC"]);
    }

    #[test]
    fn flag_one_returns_last_value() {
        let a = args(&["--equity", "1000", "--equity", "5000"]);
        assert_eq!(flag_one(&a, "--equity").as_deref(), Some("5000"));
    }

    #[test]
    fn flag_or_falls_back_to_default() {
        let a = args(&["--symbol", "ETHUSDC"]);
        assert_eq!(flag_or(&a, "--equity", "10000"), "10000");
        assert_eq!(flag_or(&a, "--symbol", "X"), "ETHUSDC");
    }

    /// 两种列表写法都要支持——脚本拼接常用重复 flag，手敲常用逗号。
    #[test]
    fn flag_list_accepts_both_comma_and_repeated_forms() {
        let a = args(&["--kind", "klines,agg_trades"]);
        assert_eq!(flag_list(&a, "--kind"), vec!["klines", "agg_trades"]);

        let b = args(&["--kind", "klines", "--kind", "agg_trades"]);
        assert_eq!(flag_list(&b, "--kind"), vec!["klines", "agg_trades"]);

        // 混合写法与空白也要处理
        let c = args(&["--kind", "klines, agg_trades", "--kind", "funding"]);
        assert_eq!(
            flag_list(&c, "--kind"),
            vec!["klines", "agg_trades", "funding"]
        );
    }

    #[test]
    fn flag_list_ignores_empty_parts() {
        let a = args(&["--kind", "klines,,agg_trades,"]);
        assert_eq!(flag_list(&a, "--kind"), vec!["klines", "agg_trades"]);
    }

    /// 缺少值时不能 panic，也不能把下一个 flag 当成值。
    #[test]
    fn flag_at_end_without_value_is_ignored() {
        let a = args(&["--symbol"]);
        assert!(flag_values(&a, "--symbol").is_empty());
    }

    #[test]
    fn parse_month_validates_format_and_range() {
        assert_eq!(parse_month("2026-08").unwrap(), (2026, 8));
        assert_eq!(parse_month("2024-01").unwrap(), (2024, 1));
        assert!(parse_month("2026").is_err());
        assert!(parse_month("2026-13").is_err(), "月份 13 应被拒绝");
        assert!(parse_month("2026-00").is_err(), "月份 0 应被拒绝");
        assert!(parse_month("abc-08").is_err());
    }

    #[test]
    fn parse_date_validates_format() {
        assert!(parse_date("2026-08-01").is_ok());
        assert!(parse_date("2026-13-01").is_err());
        assert!(parse_date("2026/08/01").is_err());
    }

    #[test]
    fn flag_parse_reports_invalid_value() {
        let a = args(&["--equity", "not-a-number"]);
        let r: Result<Option<rust_decimal::Decimal>> = flag_parse(&a, "--equity");
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("equity"));
    }

    /// 中文按 2 列宽计算——否则说明文字在终端里会溢出。
    #[test]
    fn wrap_counts_cjk_as_double_width() {
        let text = "这是一个用于测试折行的中文句子，它应该按照字符宽度被切分成多行。";
        let lines = wrap(text, 20);
        assert!(lines.len() > 1, "长文本应被折行");
        for l in &lines {
            let w: usize = l
                .chars()
                .map(|c| if (c as u32) > 0x2E80 { 2 } else { 1 })
                .sum();
            assert!(w <= 20, "折行后宽度不应超过限制：{l} ({w})");
        }
    }

    #[test]
    fn wrap_handles_short_and_empty_text() {
        assert_eq!(wrap("短", 20), vec!["短"]);
        assert!(wrap("", 20).is_empty());
    }
}
