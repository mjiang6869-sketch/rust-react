//! `rc download` —— 下载并转换历史数据。

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use data::download::{
    DEFAULT_ARCHIVE_BASE, DownloadOutcome, DownloadProgress, Layout, fetch_partition,
};
use data::manifest::{DatasetKind, Manifest, PartitionKey, PartitionStatus};
use tokio::sync::Mutex;

use crate::format;
use crate::{data_root, flag_list, flag_one, flag_parse, parse_month};

/// 每个数据集的默认下载顺序：体积小的先下。
///
/// 先下 K 线与资金费能让"数据管道是否跑通"很快得到验证，而不必等
/// 逐笔成交的大包下完。
fn kind_order() -> Vec<(DatasetKind, &'static str)> {
    vec![
        (DatasetKind::Klines1m, "klines"),
        (DatasetKind::FundingRate, "funding"),
        (DatasetKind::MarkPriceKlines1m, "mark_price"),
        (DatasetKind::AggTrades, "agg_trades"),
    ]
}

fn parse_kinds(names: &[String]) -> Result<Vec<DatasetKind>> {
    let mut out = Vec::new();
    for n in names {
        let k = match n.as_str() {
            "klines" => DatasetKind::Klines1m,
            "agg_trades" => DatasetKind::AggTrades,
            "mark_price" => DatasetKind::MarkPriceKlines1m,
            "funding" => DatasetKind::FundingRate,
            other => bail!("未知数据集：{other}。可用：klines / agg_trades / mark_price / funding"),
        };
        if !out.contains(&k) {
            out.push(k);
        }
    }
    // 按体积从小到大排序，让首个分区尽快出结果
    out.sort_by_key(|k| {
        kind_order()
            .iter()
            .position(|(kk, _)| kk == k)
            .unwrap_or(usize::MAX)
    });
    Ok(out)
}

/// 终端进度显示。
struct TermProgress {
    current: String,
}

impl DownloadProgress for TermProgress {
    fn on_start(&mut self, key: &PartitionKey, _url: &str) {
        self.current = format!("{key}");
        print!("  下载中 {} ...", self.current);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    fn on_downloaded(&mut self, _: &PartitionKey, bytes: u64, secs: u64) {
        print!(" {} MB / {}s，转换中...", format::mb(bytes), secs);
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    fn on_converted(&mut self, _: &PartitionKey, rows: u64, secs: u64) {
        println!(" {rows} 行 / {secs}s");
    }

    fn on_skip(&mut self, key: &PartitionKey) {
        // 已完成的分区不逐条打印——重复运行时会有几百行噪音。
        let _ = key;
    }

    fn on_absent(&mut self, key: &PartitionKey) {
        println!("  归档无此分区 {key}（标的可能尚未上线）");
    }

    fn on_error(&mut self, key: &PartitionKey, err: &str) {
        println!("  失败 {key}: {err}");
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let symbols = flag_list(args, "--symbol");
    if symbols.is_empty() {
        bail!("必须指定 --symbol（可重复或用逗号分隔）");
    }

    let kind_names = flag_list(args, "--kind");
    if kind_names.is_empty() {
        bail!("必须指定 --kind（klines / agg_trades / mark_price / funding）");
    }
    let kinds = parse_kinds(&kind_names)?;

    let from = parse_month(&flag_one(args, "--from").context("必须指定 --from YYYY-MM")?)?;
    let to = parse_month(&flag_one(args, "--to").context("必须指定 --to YYYY-MM")?)?;
    if from > to {
        bail!("--from ({from:?}) 不能晚于 --to ({to:?})");
    }

    let root = data_root(args);
    let concurrency: usize = flag_parse(args, "--concurrency")?.unwrap_or(4);
    if concurrency == 0 {
        bail!("--concurrency 必须大于 0");
    }

    let layout = Layout::new(&root);
    let manifest = Manifest::load(&layout.manifest_path())?;

    // 收集待办分区。台账里已完成或确认归档无此分区的会被跳过。
    let mut work: Vec<PartitionKey> = Vec::new();
    for kind in &kinds {
        for symbol in &symbols {
            for key in manifest.pending(*kind, symbol, from, to) {
                work.push(key);
            }
        }
    }

    let total = work.len();
    if total == 0 {
        println!("所有分区都已完成，无需下载。");
        println!("\n用 rc coverage 查看本地数据覆盖。");
        return Ok(());
    }

    let total_bytes_hint: u64 = work
        .iter()
        .map(|k| match k.kind {
            DatasetKind::AggTrades => 180_000_000,
            _ => 2_000_000,
        })
        .sum();

    println!("待下载 {} 个分区", total);
    format::kv("交易对", &symbols.join(", "));
    format::kv(
        "数据集",
        &kinds
            .iter()
            .map(|k| k.archive_dir().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    );
    format::kv("区间", &format!("{from:?} .. {to:?}"));
    format::kv("数据根目录", &root.display().to_string());
    format::kv(
        "预计下载量",
        &format!("约 {} MB（压缩）", format::mb(total_bytes_hint)),
    );
    format::kv("并发数", &concurrency.to_string());
    println!("\n可以随时 Ctrl-C 中断，重跑会跳过已完成的分区。\n");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .user_agent("rust-crypto-research/0.1")
        .build()
        .context("构造 HTTP 客户端失败")?;

    // 台账由多任务共享，每完成一个分区就落盘——这样中断时进度不丢。
    let manifest = Arc::new(Mutex::new(manifest));
    let layout = Arc::new(layout);
    let client = Arc::new(client);

    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let skipped = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut handles = Vec::new();
    for key in work {
        let sem = sem.clone();
        let client = client.clone();
        let layout = layout.clone();
        let manifest = manifest.clone();
        let done = done.clone();
        let failed = failed.clone();
        let skipped = skipped.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("信号量不会关闭");

            let mut progress = TermProgress {
                current: String::new(),
            };
            let result =
                fetch_partition(&client, &layout, &key, DEFAULT_ARCHIVE_BASE, &mut progress).await;

            let mut guard = manifest.lock().await;
            match result {
                Ok((outcome, entry)) => {
                    let is_skip = matches!(outcome, DownloadOutcome::Skipped);
                    let status = entry.status.clone();
                    guard.record(key.clone(), entry);
                    if is_skip {
                        skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let PartitionStatus::Suspicious {
                        row_count,
                        expected,
                        ..
                    } = status
                    {
                        println!(
                            "  注意：{key} 行数 {row_count} 与期望 {expected} 不符，已标记为待查。\
                             它不会被当作可用数据，重跑会重下。"
                        );
                    }
                }
                Err(e) => {
                    let attempts = match guard.entry(&key).map(|x| &x.status) {
                        Some(PartitionStatus::Failed { attempts, .. }) => attempts + 1,
                        _ => 1,
                    };
                    guard.record(
                        key.clone(),
                        data::PartitionEntry {
                            status: PartitionStatus::Failed {
                                error: format!("{e:#}"),
                                attempts,
                            },
                            ..data::PartitionEntry::absent()
                        },
                    );
                    failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    eprintln!("  失败 {key}（第 {attempts} 次）：{e:#}");
                }
            }

            // 每个分区完成即落盘。中断时已完成的进度不会丢。
            let mut m = guard;
            if let Err(e) = m.save(&layout.manifest_path()) {
                eprintln!("  警告：台账保存失败 {e:#}");
            }

            let n = done.load(std::sync::atomic::Ordering::Relaxed)
                + failed.load(std::sync::atomic::Ordering::Relaxed);
            if n % 10 == 0 || n == total {
                println!("  进度 {n}/{total}");
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let done = done.load(std::sync::atomic::Ordering::Relaxed);
    let failed = failed.load(std::sync::atomic::Ordering::Relaxed);

    println!();
    format::rule(60);
    format::kv("完成", &done.to_string());
    format::kv("失败", &failed.to_string());
    if failed > 0 {
        println!("\n失败的分区可以重跑同一条命令——已完成的部分会被跳过。");
    }
    println!("\n用 rc coverage 查看覆盖情况。");

    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parse_kinds_maps_names_and_dedupes() {
        let k = parse_kinds(&args(&["klines", "agg_trades", "klines"])).unwrap();
        assert_eq!(k, vec![DatasetKind::Klines1m, DatasetKind::AggTrades]);
    }

    /// 下载顺序按体积从小到大——先下小文件能让管道验证尽快出结果。
    #[test]
    fn kinds_are_ordered_smallest_first() {
        let k = parse_kinds(&args(&["agg_trades", "klines", "funding"])).unwrap();
        assert_eq!(
            k,
            vec![
                DatasetKind::Klines1m,
                DatasetKind::FundingRate,
                DatasetKind::AggTrades
            ],
            "逐笔成交（最大）应排最后"
        );
    }

    #[test]
    fn unknown_kind_is_rejected_with_helpful_message() {
        let e = parse_kinds(&args(&["klienes"])).unwrap_err().to_string();
        assert!(e.contains("klienes"), "{e}");
        assert!(e.contains("klines"), "错误信息应列出可用选项：{e}");
    }
}
