//! `rc download` —— 下载并转换历史数据。
//!
//! # `earliest` / `latest`
//!
//! `--from earliest`：以所选数据集在归档里实际存在的最早月份为起点。
//! `--to latest`：以所选数据集在归档里实际存在的最晚月份为终点。
//! 两者都要求先对每个 (数据集, 交易对) 组合列举归档——列举失败时无法安全
//! 确定边界，会直接报错退出，提示改用具体月份。
//!
//! # 归档索引与裁剪
//!
//! 不管是否用了 `earliest`/`latest`，下载前都会对每个 (数据集, 交易对)
//! 组合列举一次真实归档范围（间隔 200ms，避免无间隔连续请求），交给
//! [`data::plan_work`] 裁剪请求区间——避免对超出归档范围的月份逐个发起
//! 注定 404 的请求，也避免把这些请求写进台账。某个组合列举失败时，
//! 对应位置传 `None`，`plan_work` 会退化为"裁到上个月"的保守策略。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use data::download::{
    CancelToken, DEFAULT_ARCHIVE_BASE, DownloadProgress, Layout, Stage, fetch_partition,
};
use data::manifest::{DatasetKind, Manifest, PartitionEntry, PartitionKey, PartitionStatus};
use data::{ArchiveMonths, fetch_archive_months, plan_work};
use tokio::sync::Mutex;

use crate::format;
use crate::{data_root, flag_list, flag_one, flag_parse, parse_month};

/// 归档索引查询之间的固定间隔，避免对 S3 无间隔连续请求。
const INDEX_QUERY_INTERVAL: Duration = Duration::from_millis(200);

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
        let k = DatasetKind::from_api_name(n).map_err(|e| anyhow::anyhow!(e))?;
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

/// `--from`/`--to` 解析结果：具体月份，或"归档里实际存在的最早/最晚月份"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonthArg {
    Month(i32, u32),
    Earliest,
    Latest,
}

/// 解析 `--from`/`--to` 的原始字符串。
fn parse_month_arg(s: &str) -> Result<MonthArg> {
    match s {
        "earliest" => Ok(MonthArg::Earliest),
        "latest" => Ok(MonthArg::Latest),
        other => parse_month(other)
            .map(|(y, m)| MonthArg::Month(y, m))
            .map_err(|_| {
                anyhow::anyhow!("--from/--to 的值应为 YYYY-MM、earliest 或 latest，收到：{other}")
            }),
    }
}

/// 从多个归档月份集合里取最早的月份。纯函数，不做任何 IO。
fn earliest_across(all: &[ArchiveMonths]) -> Option<(i32, u32)> {
    all.iter().filter_map(ArchiveMonths::earliest).min()
}

/// 从多个归档月份集合里取最晚的月份。纯函数，不做任何 IO。
fn latest_across(all: &[ArchiveMonths]) -> Option<(i32, u32)> {
    all.iter().filter_map(ArchiveMonths::latest).max()
}

/// 终端进度显示。
struct TermProgress {
    current: String,
    last_stage: Option<Stage>,
}

impl DownloadProgress for TermProgress {
    fn on_start(&mut self, key: &PartitionKey, _url: &str) {
        self.current = format!("{key}");
        self.last_stage = None;
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

    fn on_stage(&mut self, _: &PartitionKey, stage: Stage, _done: u64, _total: Option<u64>) {
        if self.last_stage != Some(stage) {
            self.last_stage = Some(stage);
            println!();
            print!("  [{}] {} ...", self.current, stage.label());
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
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

    let from_raw = flag_one(args, "--from").context("必须指定 --from（YYYY-MM 或 earliest）")?;
    let to_raw = flag_one(args, "--to").context("必须指定 --to（YYYY-MM 或 latest）")?;
    let from_arg = parse_month_arg(&from_raw)?;
    let to_arg = parse_month_arg(&to_raw)?;
    if matches!(from_arg, MonthArg::Latest) {
        bail!("--from 不支持 latest，只接受 YYYY-MM 或 earliest");
    }
    if matches!(to_arg, MonthArg::Earliest) {
        bail!("--to 不支持 earliest，只接受 YYYY-MM 或 latest");
    }

    let root = data_root(args);
    let concurrency: usize = flag_parse(args, "--concurrency")?.unwrap_or(4);
    if concurrency == 0 {
        bail!("--concurrency 必须大于 0");
    }

    let layout = Layout::new(&root);
    let manifest = Manifest::load(&layout.manifest_path())?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .user_agent("rust-crypto-research/0.1")
        .build()
        .context("构造 HTTP 客户端失败")?;

    // ---- 归档索引：对每个 (数据集, 交易对) 串行列举，间隔 200ms ----
    let mut index_cache: HashMap<(DatasetKind, String), Option<ArchiveMonths>> = HashMap::new();
    let mut first = true;
    for &kind in &kinds {
        for symbol in &symbols {
            if !first {
                tokio::time::sleep(INDEX_QUERY_INTERVAL).await;
            }
            first = false;
            let months = match fetch_archive_months(&client, kind, symbol).await {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("  警告：列举 {symbol} 的 {kind:?} 归档失败：{e:#}");
                    None
                }
            };
            index_cache.insert((kind, symbol.clone()), months);
        }
    }

    let needs_earliest = matches!(from_arg, MonthArg::Earliest);
    let needs_latest = matches!(to_arg, MonthArg::Latest);
    if (needs_earliest || needs_latest) && index_cache.values().any(|v| v.is_none()) {
        bail!("无法获取归档范围，请改用具体月份（例如 --from 2024-01）");
    }

    let all_months: Vec<ArchiveMonths> = index_cache.values().filter_map(|v| v.clone()).collect();

    let from = match from_arg {
        MonthArg::Month(y, m) => (y, m),
        MonthArg::Earliest => earliest_across(&all_months)
            .context("无法获取归档范围，请改用具体月份（例如 --from 2024-01）")?,
        MonthArg::Latest => unreachable!("已在上面拒绝 --from latest"),
    };
    let to = match to_arg {
        MonthArg::Month(y, m) => (y, m),
        MonthArg::Latest => latest_across(&all_months)
            .context("无法获取归档范围，请改用具体月份（例如 --to 2026-08）")?,
        MonthArg::Earliest => unreachable!("已在上面拒绝 --to earliest"),
    };
    if from > to {
        bail!("--from ({from:?}) 不能晚于 --to ({to:?})");
    }

    let index_fn = |kind: DatasetKind, symbol: &str| -> Option<ArchiveMonths> {
        index_cache
            .get(&(kind, symbol.to_string()))
            .cloned()
            .flatten()
    };

    let today = Utc::now().date_naive();
    let plan = plan_work(&manifest, &kinds, &symbols, from, to, &index_fn, today);
    for note in &plan.clipped {
        println!("  注意：{note}");
    }

    let work = plan.work;
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

    // 单一取消令牌，供 Ctrl-C 监听与全部并发下载任务共享。
    let cancel = CancelToken::default();
    {
        let cancel_for_signal = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel_for_signal.cancel();
                println!("\n正在取消，已下载的部分会保留以便续传...");
            }
        });
    }

    // 台账由多任务共享，每完成一个分区就用 record_and_save 落盘——这样
    // 中断时进度不丢，也不会让多个任务的整份保存互相覆盖。
    let manifest = Arc::new(Mutex::new(manifest));
    let layout = Arc::new(layout);
    let client = Arc::new(client);

    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let done = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let cancelled_count = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for key in work {
        let sem = sem.clone();
        let client = client.clone();
        let layout = layout.clone();
        let manifest = manifest.clone();
        let done = done.clone();
        let failed = failed.clone();
        let cancelled_count = cancelled_count.clone();
        let cancel = cancel.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("信号量不会关闭");

            let mut progress = TermProgress {
                current: String::new(),
                last_stage: None,
            };
            let result = fetch_partition(
                &client,
                &layout,
                &key,
                DEFAULT_ARCHIVE_BASE,
                &mut progress,
                &cancel,
            )
            .await;

            match result {
                Ok((_outcome, entry)) => {
                    let status = entry.status.clone();
                    {
                        let mut guard = manifest.lock().await;
                        if let Err(e) =
                            guard.record_and_save(&layout.manifest_path(), key.clone(), entry)
                        {
                            eprintln!("  警告：台账保存失败 {e:#}");
                        }
                    }
                    done.fetch_add(1, Ordering::Relaxed);
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
                    if cancel.is_cancelled() {
                        // 取消导致的失败：不写台账，也不计入失败——半途而废
                        // 的分区不应该占用重试计数，重跑会重新下载它。
                        cancelled_count.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    let attempts = {
                        let guard = manifest.lock().await;
                        match guard.entry(&key).map(|x| &x.status) {
                            Some(PartitionStatus::Failed { attempts, .. }) => attempts + 1,
                            _ => 1,
                        }
                    };
                    let failed_entry = PartitionEntry {
                        status: PartitionStatus::Failed {
                            error: format!("{e:#}"),
                            attempts,
                        },
                        ..PartitionEntry::absent()
                    };
                    {
                        let mut guard = manifest.lock().await;
                        if let Err(e2) = guard.record_and_save(
                            &layout.manifest_path(),
                            key.clone(),
                            failed_entry,
                        ) {
                            eprintln!("  警告：台账保存失败 {e2:#}");
                        }
                    }
                    failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("  失败 {key}（第 {attempts} 次）：{e:#}");
                }
            }

            let n = done.load(Ordering::Relaxed) + failed.load(Ordering::Relaxed);
            if n % 10 == 0 || n == total {
                println!("  进度 {n}/{total}");
            }
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let done = done.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    let cancelled_count = cancelled_count.load(Ordering::Relaxed);
    let was_cancelled = cancel.is_cancelled();

    println!();
    format::rule(60);
    format::kv("完成", &done.to_string());
    format::kv("失败", &failed.to_string());
    if was_cancelled {
        format::kv("已取消", &cancelled_count.to_string());
    }
    if failed > 0 {
        println!("\n失败的分区可以重跑同一条命令——已完成的部分会被跳过。");
    }
    println!("\n用 rc coverage 查看覆盖情况。");

    if was_cancelled || failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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

    // ---------------- MonthArg ----------------

    #[test]
    fn parse_month_arg_accepts_explicit_month() {
        assert_eq!(
            parse_month_arg("2024-01").unwrap(),
            MonthArg::Month(2024, 1)
        );
    }

    #[test]
    fn parse_month_arg_accepts_earliest_and_latest() {
        assert_eq!(parse_month_arg("earliest").unwrap(), MonthArg::Earliest);
        assert_eq!(parse_month_arg("latest").unwrap(), MonthArg::Latest);
    }

    #[test]
    fn parse_month_arg_rejects_invalid_value_with_chinese_message() {
        let err = parse_month_arg("soon").unwrap_err().to_string();
        assert!(err.contains("YYYY-MM"), "{err}");
        assert!(err.contains("earliest"), "{err}");
        assert!(err.contains("latest"), "{err}");
    }

    // ---------------- earliest_across / latest_across ----------------

    #[test]
    fn earliest_across_picks_the_minimum_across_all_datasets() {
        let a = ArchiveMonths {
            months: BTreeSet::from([(2024, 3), (2024, 1)]),
        };
        let b = ArchiveMonths {
            months: BTreeSet::from([(2023, 12), (2024, 5)]),
        };
        assert_eq!(earliest_across(&[a, b]), Some((2023, 12)));
    }

    #[test]
    fn latest_across_picks_the_maximum_across_all_datasets() {
        let a = ArchiveMonths {
            months: BTreeSet::from([(2024, 3), (2024, 1)]),
        };
        let b = ArchiveMonths {
            months: BTreeSet::from([(2023, 12), (2024, 5)]),
        };
        assert_eq!(latest_across(&[a, b]), Some((2024, 5)));
    }

    #[test]
    fn earliest_and_latest_across_empty_input_is_none() {
        assert_eq!(earliest_across(&[]), None);
        assert_eq!(latest_across(&[]), None);
    }
}
