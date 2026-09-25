//! 归档下载器。
//!
//! # 设计要点
//!
//! 1. **归档优先**：币安归档是 CDN 上的静态 ZIP，无速率限制、支持 HTTP Range
//!    断点续传。用 REST 拉多年历史会撞上权重限制并有封禁风险。
//! 2. **流式**：下载 → 解压 → 转 Parquet → 校验 → 删除中间文件，全部逐个
//!    分区进行。三标的 32 个月的 CSV 总量超过 55 GB，不能先全下再统一转换。
//! 3. **可中断**：每个分区的完成状态写入台账。重跑时已完成的分区直接跳过。
//! 4. **按数据集分目录解压**：这一条是实际踩过的坑——`klines` 与
//!    `markPriceKlines` 解压后得到**同名 CSV**，同目录解压会让后者静默覆盖
//!    前者。我在验证时正是因此把标记价当成真实 K 线，得出了错误结论。
//! 5. **校验 sha256**：归档自带 `.CHECKSUM` 文件。校验失败的分区拒绝入库，
//!    否则会在后续所有回测里静默使用损坏数据。

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::manifest::{
    DatasetKind, PartitionEntry, PartitionKey, PartitionStatus, archive_url_and_name,
};
use crate::parquet_writer::convert_csv_to_parquet;

/// 归档基地址。
pub const DEFAULT_ARCHIVE_BASE: &str = "https://data.binance.vision/data/futures/um/monthly";

/// 下载器的磁盘布局。
#[derive(Clone, Debug)]
pub struct Layout {
    /// 数据根目录。
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 原始 ZIP 的存放路径。
    pub fn raw_zip(&self, key: &PartitionKey) -> PathBuf {
        self.root
            .join("raw")
            .join("binance/um")
            .join(key.kind.archive_dir())
            .join(&key.symbol)
            .join(format!("{}-{:02}", key.year, key.month))
            .join(format!(
                "{}-{}-{}-{:02}.zip",
                key.symbol,
                key.kind.file_tag(),
                key.year,
                key.month
            ))
    }

    /// CSV 解压目录。
    ///
    /// **按 dataset kind 分开**——不同数据集的归档内 CSV 可能重名。
    pub fn csv_dir(&self, key: &PartitionKey) -> PathBuf {
        self.root
            .join("tmp")
            .join("csv")
            .join(key.kind.archive_dir())
            .join(&key.symbol)
            .join(format!("{}-{:02}", key.year, key.month))
    }

    /// 转换后的 Parquet 路径。
    pub fn parquet(&self, key: &PartitionKey) -> PathBuf {
        match key.kind {
            // K 线按月分区
            DatasetKind::Klines1m | DatasetKind::MarkPriceKlines1m => self
                .root
                .join("lake")
                .join(key.kind.archive_dir())
                .join(format!("symbol={}", key.symbol))
                .join(format!("interval={}", key.kind.file_tag()))
                .join(format!("year={}", key.year))
                .join(format!("month={:02}", key.month))
                .join("data.parquet"),
            // 逐笔与资金费也按月分区（下载粒度是月）
            DatasetKind::AggTrades | DatasetKind::FundingRate => self
                .root
                .join("lake")
                .join(key.kind.archive_dir())
                .join(format!("symbol={}", key.symbol))
                .join(format!("year={}", key.year))
                .join(format!("month={:02}", key.month))
                .join("data.parquet"),
        }
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest").join("manifest.json")
    }
}

/// 一次分区下载的结果。
#[derive(Clone, Debug)]
pub enum DownloadOutcome {
    /// 下载并转换成功。
    Done { rows: u64, parquet_bytes: u64 },
    /// 归档里没有这个分区（标的尚未上线等）。这是终态。
    NotInArchive,
    /// 跳过（已完成）。
    Skipped,
}

/// 下载器的进度回调。
pub trait DownloadProgress: Send {
    fn on_start(&mut self, key: &PartitionKey, url: &str);
    fn on_downloaded(&mut self, key: &PartitionKey, bytes: u64, secs: u64);
    fn on_converted(&mut self, key: &PartitionKey, rows: u64, secs: u64);
    fn on_skip(&mut self, key: &PartitionKey);
    fn on_absent(&mut self, key: &PartitionKey);
    fn on_error(&mut self, key: &PartitionKey, err: &str);
}

/// 无操作回调。
pub struct QuietProgress;
impl DownloadProgress for QuietProgress {
    fn on_start(&mut self, _: &PartitionKey, _: &str) {}
    fn on_downloaded(&mut self, _: &PartitionKey, _: u64, _: u64) {}
    fn on_converted(&mut self, _: &PartitionKey, _: u64, _: u64) {}
    fn on_skip(&mut self, _: &PartitionKey) {}
    fn on_absent(&mut self, _: &PartitionKey) {}
    fn on_error(&mut self, _: &PartitionKey, _: &str) {}
}

/// 下载并转换一个分区。
///
/// 步骤：下载 ZIP → 校验 sha256 → 解压 CSV 到独立的按数据集目录 → 转 Parquet
/// → 清理中间文件。任一步失败都不会把分区标记为完成。
pub async fn fetch_partition(
    client: &reqwest::Client,
    layout: &Layout,
    key: &PartitionKey,
    base_url: &str,
    progress: &mut dyn DownloadProgress,
) -> Result<(DownloadOutcome, PartitionEntry)> {
    let (url, _name) = archive_url_and_name(base_url, key);
    progress.on_start(key, &url);

    let zip_path = layout.raw_zip(key);
    if let Some(parent) = zip_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // ---- 1. 下载（带断点续传）----
    let started = std::time::Instant::now();
    let mut existing = 0u64;
    if let Ok(md) = tokio::fs::metadata(&zip_path).await {
        existing = md.len();
    }

    let mut req = client.get(&url);
    if existing > 0 {
        req = req.header("Range", format!("bytes={existing}-"));
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("请求失败: {url}"))?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        // 归档里没有这个分区。这是终态，不是错误——标的可能当时还没上线。
        progress.on_absent(key);
        return Ok((
            DownloadOutcome::NotInArchive,
            PartitionEntry {
                status: PartitionStatus::NotInArchive,
                ..PartitionEntry::absent()
            },
        ));
    }
    if !status.is_success() {
        bail!("HTTP {} 下载 {}", status, url);
    }

    {
        let append = existing > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        let mut file = if append {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&zip_path)
                .await?
        } else {
            tokio::fs::File::create(&zip_path).await?
        };
        let mut stream = resp.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("下载流中断")?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
    }

    let zip_bytes = tokio::fs::metadata(&zip_path).await?.len();
    let dl_secs = started.elapsed().as_secs();
    progress.on_downloaded(key, zip_bytes, dl_secs);

    // ---- 2. 校验 sha256 ----
    let sha = sha256_file(&zip_path)?;
    if let Some(expected) = fetch_expected_checksum(client, &url).await {
        if !checksum_matches(&expected, &sha) {
            // 校验失败必须删除文件并报错——留在磁盘上会在下次重跑时
            // 被当成本地缓存复用。
            let _ = tokio::fs::remove_file(&zip_path).await;
            bail!("校验和不符：期望 {expected}，实际 {sha}。文件已删除，请重试。");
        }
    }

    // ---- 3. 解压到独立目录 ----
    let conv_started = std::time::Instant::now();
    let csv_dir = layout.csv_dir(key);
    tokio::fs::create_dir_all(&csv_dir).await?;
    let csv_path = extract_single_csv(&zip_path, &csv_dir)?;

    // ---- 4. 转 Parquet ----
    let parquet_path = layout.parquet(key);
    if let Some(parent) = parquet_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let stats = convert_csv_to_parquet(key.kind, &csv_path, &parquet_path)?;
    let parquet_bytes = std::fs::metadata(&parquet_path)?.len();
    progress.on_converted(key, stats.row_count, conv_started.elapsed().as_secs());

    // ---- 5. 校验行数与粒度期望 ----
    let status = match key.kind.expected_rows_per_day() {
        Some(per_day) => {
            let days = super::replay::days_in_month(key.year, key.month) as u64;
            let expected = per_day * days;
            let tolerance = tolerance_for(expected);
            if stats.row_count.abs_diff(expected) <= tolerance {
                PartitionStatus::Finalized {
                    row_count: stats.row_count,
                    min_ts: stats.min_ts,
                    max_ts: stats.max_ts,
                }
            } else {
                // 行数不符**不标记通过**——宁可重复下载，也不能把不完整
                // 数据当成完整数据用。
                PartitionStatus::Suspicious {
                    row_count: stats.row_count,
                    expected,
                    note: format!("行数与 {per_day}×{days} 天 = {expected} 相差超过容差"),
                }
            }
        }
        // 逐笔成交没有固定行数期望，不假装有。只要转换成功就算完成。
        None => PartitionStatus::Finalized {
            row_count: stats.row_count,
            min_ts: stats.min_ts,
            max_ts: stats.max_ts,
        },
    };

    // ---- 6. 清理中间文件 ----
    // 保留 ZIP（用于校验与重新转换），删除体积更大的 CSV 目录。
    // ZIP 是压缩的，CSV 解压后膨胀 5 倍左右，删掉它能省下大量空间。
    let _ = tokio::fs::remove_dir_all(&csv_dir).await;

    let outcome = match &status {
        PartitionStatus::Finalized { row_count, .. } => DownloadOutcome::Done {
            rows: *row_count,
            parquet_bytes,
        },
        PartitionStatus::Suspicious { row_count, .. } => DownloadOutcome::Done {
            rows: *row_count,
            parquet_bytes,
        },
        _ => DownloadOutcome::NotInArchive,
    };

    Ok((
        outcome,
        PartitionEntry {
            status,
            source_sha256: Some(sha),
            source_url: Some(url),
            parquet_path: Some(
                parquet_path
                    .strip_prefix(&layout.root)
                    .unwrap_or(&parquet_path)
                    .to_path_buf(),
            ),
            parquet_bytes: Some(parquet_bytes),
            fetched_at: Some(Utc::now()),
            restated_at: None,
        },
    ))
}

/// 行数容差。
///
/// K 线允许极少量缺失（币安偶发漏根），但不容忍大范围缺失。容差取
/// 期望值的 1% 与 5 根中的较大者——前者容忍偶发漏根，后者容忍测试或
/// 短分区的边界情况。
fn tolerance_for(expected: u64) -> u64 {
    (expected / 100).max(5)
}

/// 计算文件 sha256。
fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 取归档的 `.CHECKSUM` 内容。取不到时返回 `None`（不阻断下载）。
async fn fetch_expected_checksum(client: &reqwest::Client, zip_url: &str) -> Option<String> {
    let resp = client
        .get(format!("{zip_url}.CHECKSUM"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    // 格式：`<sha256>  <filename>`
    text.split_whitespace().next().map(|s| s.to_string())
}

/// 比对校验和。大小写不敏感。
fn checksum_matches(expected: &str, actual: &str) -> bool {
    expected.eq_ignore_ascii_case(actual)
}

/// 从 ZIP 中提取唯一的 CSV 文件。
///
/// 币安归档每个包只含一个 CSV，但文件名不含数据集标识（`ETHUSDC-1m-2026-08.csv`
/// 在 klines 与 markPriceKlines 里完全同名）。所以解压目标目录必须由调用方
/// 按数据集分开——这里只负责提取，不负责防止覆盖。
fn extract_single_csv(zip_path: &Path, out_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("打开 ZIP 失败: {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("解析 ZIP 失败")?;

    if archive.is_empty() {
        bail!("ZIP 为空: {}", zip_path.display());
    }

    // 找第一个 .csv 条目
    let mut target: Option<String> = None;
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        if entry.name().to_ascii_lowercase().ends_with(".csv") {
            target = Some(entry.name().to_string());
            break;
        }
    }
    let Some(name) = target else {
        bail!("ZIP 内没有 CSV: {}", zip_path.display());
    };

    let out_path = out_dir.join(Path::new(&name).file_name().context("非法文件名")?);
    {
        let mut entry = archive.by_name(&name)?;
        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("创建 CSV 失败: {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
    }
    Ok(out_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kind: DatasetKind, month: u32) -> PartitionKey {
        PartitionKey {
            kind,
            symbol: "ETHUSDC".into(),
            year: 2026,
            month,
        }
    }

    /// 这是我在验证时踩到的坑：klines 与 markPriceKlines 的归档内 CSV 同名。
    /// 所以解压目录必须按数据集分开，否则后者会静默覆盖前者。
    #[test]
    fn csv_dirs_are_separated_by_dataset() {
        let layout = Layout::new("/data");
        let k = layout.csv_dir(&key(DatasetKind::Klines1m, 8));
        let m = layout.csv_dir(&key(DatasetKind::MarkPriceKlines1m, 8));
        assert_ne!(
            k, m,
            "不同数据集的解压目录必须不同，否则同名 CSV 会互相覆盖"
        );
        assert!(k.to_string_lossy().contains("klines"));
        assert!(m.to_string_lossy().contains("markPriceKlines"));
    }

    #[test]
    fn parquet_paths_use_hive_partitioning() {
        let layout = Layout::new("/data");
        let p = layout.parquet(&key(DatasetKind::Klines1m, 8));
        let s = p.to_string_lossy();
        assert!(s.contains("symbol=ETHUSDC"), "{s}");
        assert!(s.contains("interval=1m"), "{s}");
        assert!(s.contains("year=2026"), "{s}");
        assert!(s.contains("month=08"), "{s}");
        assert!(s.ends_with("data.parquet"), "{s}");
    }

    /// 月份必须零填充，否则 `month=8` 与 `month=08` 会被当成两个分区。
    #[test]
    fn month_is_zero_padded() {
        let layout = Layout::new("/data");
        let p = layout.parquet(&key(DatasetKind::AggTrades, 3));
        assert!(p.to_string_lossy().contains("month=03"), "{p:?}");
    }

    #[test]
    fn raw_zip_path_is_layout_stable() {
        let layout = Layout::new("/data");
        let z = layout.raw_zip(&key(DatasetKind::AggTrades, 8));
        let s = z.to_string_lossy();
        assert!(
            s.contains("raw/binance/um/aggTrades/ETHUSDC/2026-08"),
            "{s}"
        );
        assert!(s.ends_with("ETHUSDC-aggTrades-2026-08.zip"), "{s}");
    }

    #[test]
    fn checksum_comparison_is_case_insensitive() {
        assert!(checksum_matches("ABC123", "abc123"));
        assert!(checksum_matches("abc123", "abc123"));
        assert!(!checksum_matches("abc123", "abd123"));
    }

    /// 容差要小到能发现大范围缺失，大到能容忍偶发漏根。
    #[test]
    fn row_count_tolerance_balances_both_risks() {
        // 44640 根（31 天）：1% 容差 = 446
        let t = tolerance_for(44_640);
        assert_eq!(t, 446);
        assert!(t < 44_640 / 10, "容差不能大到掩盖大范围缺失");

        // 极小的期望值也有下限，避免边界情况被误判
        assert_eq!(tolerance_for(3), 5);
        assert_eq!(tolerance_for(100), 5);
    }

    #[test]
    fn sha256_of_known_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.txt");
        std::fs::write(&p, b"abc").unwrap();
        // sha256("abc") 是已知常量
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
