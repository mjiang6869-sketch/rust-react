//! 后台下载任务。
//!
//! # 与 CLI 的 `download` 子命令共用下载器
//!
//! 这里只做编排与进度上报——真正的下载、校验、转换逻辑在
//! `data::download::fetch_partition`，与 CLI 完全一致。
//!
//! # 进度上报
//!
//! 每个分区完成时通过广播通道推给 WebSocket。**已完成的进度会写台账**，
//! 所以中断后重跑会跳过已完成的分区——这个特性对几小时级别的全量下载很重要。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use data::download::{
    DEFAULT_ARCHIVE_BASE, DownloadOutcome, DownloadProgress, Layout, fetch_partition,
};
use data::manifest::{DatasetKind, Manifest, PartitionKey, PartitionStatus};
use tokio::sync::Mutex;

use crate::state::ProgressMessage;

/// 静默进度回调（进度通过广播通道上报，不打印到 stdout）。
struct SilentProgress;

impl DownloadProgress for SilentProgress {
    fn on_start(&mut self, _: &PartitionKey, _: &str) {}
    fn on_downloaded(&mut self, _: &PartitionKey, _: u64, _: u64) {}
    fn on_converted(&mut self, _: &PartitionKey, _: u64, _: u64) {}
    fn on_skip(&mut self, _: &PartitionKey) {}
    fn on_absent(&mut self, _: &PartitionKey) {}
    fn on_error(&mut self, _: &PartitionKey, _: &str) {}
}

fn parse_kind(name: &str) -> Result<DatasetKind> {
    match name {
        "klines" => Ok(DatasetKind::Klines1m),
        "agg_trades" => Ok(DatasetKind::AggTrades),
        "mark_price" => Ok(DatasetKind::MarkPriceKlines1m),
        "funding" => Ok(DatasetKind::FundingRate),
        other => bail!("未知数据集：{other}。可用：klines / agg_trades / mark_price / funding"),
    }
}

/// 执行下载任务。
pub async fn run(
    data_root: PathBuf,
    symbols: Vec<String>,
    kinds: Vec<String>,
    from: (i32, u32),
    to: (i32, u32),
    progress: tokio::sync::broadcast::Sender<ProgressMessage>,
) -> Result<()> {
    let parsed_kinds: Vec<DatasetKind> = kinds
        .iter()
        .map(|k| parse_kind(k))
        .collect::<Result<Vec<_>>>()?;
    if parsed_kinds.is_empty() {
        bail!("必须指定至少一个数据集");
    }

    let layout = Layout::new(&data_root);
    let manifest = Manifest::load(&layout.manifest_path())?;

    // 收集待办
    let mut work: Vec<PartitionKey> = Vec::new();
    for kind in &parsed_kinds {
        for symbol in &symbols {
            for key in manifest.pending(*kind, symbol, from, to) {
                work.push(key);
            }
        }
    }

    let total = work.len();
    if total == 0 {
        let _ = progress.send(ProgressMessage::DownloadDone {
            completed: 0,
            failed: 0,
        });
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .user_agent("rust-crypto-research/0.1")
        .build()
        .context("构造 HTTP 客户端失败")?;

    let manifest = Arc::new(Mutex::new(manifest));
    let mut completed = 0usize;
    let mut failed = 0usize;

    // 串行下载。
    //
    // 不做并发：并发会让进度上报变成乱序，而磁盘 IO 与 Parquet 转换本身
    // 是瓶颈（转换 CPU 密集）。串行也让中断恢复的语义更简单。
    for (i, key) in work.iter().enumerate() {
        let _ = progress.send(ProgressMessage::Download {
            symbol: key.symbol.clone(),
            kind: key.kind.archive_dir().to_string(),
            month: format!("{}-{:02}", key.year, key.month),
            done: i,
            total,
        });

        let mut silent = SilentProgress;
        let result =
            fetch_partition(&client, &layout, key, DEFAULT_ARCHIVE_BASE, &mut silent).await;

        let mut guard = manifest.lock().await;
        match result {
            Ok((outcome, entry)) => {
                let suspicious = matches!(entry.status, PartitionStatus::Suspicious { .. });
                guard.record(key.clone(), entry);
                match outcome {
                    DownloadOutcome::Done { .. } => completed += 1,
                    DownloadOutcome::NotInArchive => completed += 1,
                    DownloadOutcome::Skipped => {}
                }
                if suspicious {
                    tracing::warn!("分区 {key} 行数与期望不符，已标记待查");
                }
            }
            Err(e) => {
                let attempts = match guard.entry(key).map(|x| &x.status) {
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
                failed += 1;
                tracing::warn!("分区 {key} 下载失败：{e:#}");
            }
        }

        // 每个分区完成即落盘——中断时已完成的进度不丢。
        if let Err(e) = guard.save(&layout.manifest_path()) {
            tracing::warn!("台账保存失败：{e:#}");
        }
    }

    let _ = progress.send(ProgressMessage::DownloadDone { completed, failed });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parsing_covers_all_supported_datasets() {
        assert_eq!(parse_kind("klines").unwrap(), DatasetKind::Klines1m);
        assert_eq!(parse_kind("agg_trades").unwrap(), DatasetKind::AggTrades);
        assert_eq!(
            parse_kind("mark_price").unwrap(),
            DatasetKind::MarkPriceKlines1m
        );
        assert_eq!(parse_kind("funding").unwrap(), DatasetKind::FundingRate);
    }

    #[test]
    fn unknown_kind_lists_valid_options() {
        let e = parse_kind("klienes").unwrap_err().to_string();
        assert!(e.contains("klienes"), "{e}");
        assert!(e.contains("klines"), "错误应列出可用选项：{e}");
    }
}
