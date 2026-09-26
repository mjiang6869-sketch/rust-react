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

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate, Utc};
use sha2::{Digest, Sha256};

use crate::manifest::{
    DatasetKind, PartitionEntry, PartitionKey, PartitionStatus, archive_url_and_name,
};
use crate::parquet_writer::convert_csv_to_parquet_with_progress;

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

/// `fetch_partition` 内部的阶段，用于细粒度进度上报。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// 下载 ZIP。
    Downloading,
    /// 校验 sha256。
    Verifying,
    /// 解压 CSV。
    Extracting,
    /// 转换为 Parquet。
    Converting,
}

impl Stage {
    /// 中文展示名，供终端/前端直接显示。
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Downloading => "下载",
            Stage::Verifying => "校验",
            Stage::Extracting => "解压",
            Stage::Converting => "转换",
        }
    }

    /// 小写英文标签，供日志/机器可读场景使用。
    pub fn tag(&self) -> &'static str {
        match self {
            Stage::Downloading => "downloading",
            Stage::Verifying => "verifying",
            Stage::Extracting => "extracting",
            Stage::Converting => "converting",
        }
    }
}

/// 协作式取消令牌。
///
/// `clone()` 出的每个副本共享同一个底层标记：任一副本调用 `cancel()`，
/// 所有副本的 `is_cancelled()` 都会立即观察到。
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// 请求取消。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// 是否已被请求取消。
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 下载器的进度回调。
pub trait DownloadProgress: Send {
    fn on_start(&mut self, key: &PartitionKey, url: &str);
    fn on_downloaded(&mut self, key: &PartitionKey, bytes: u64, secs: u64);
    fn on_converted(&mut self, key: &PartitionKey, rows: u64, secs: u64);
    fn on_skip(&mut self, key: &PartitionKey);
    fn on_absent(&mut self, key: &PartitionKey);
    fn on_error(&mut self, key: &PartitionKey, err: &str);
    /// 阶段内细粒度进度（字节数）。`total` 未知时为 `None`。
    ///
    /// 默认空实现——现有的 `DownloadProgress` 实现不需要跟着改。
    fn on_stage(&mut self, _key: &PartitionKey, _stage: Stage, _done: u64, _total: Option<u64>) {}
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

/// HTTP 响应分类的结果。
///
/// 把"服务器返回了什么状态码"翻译成"下载器该做什么"，与网络请求本身
/// 解耦，方便对每种组合单独测试（尤其是容易被忽略的 416/404 边界）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RespAction {
    /// 全新下载（覆盖写）。
    Fresh,
    /// 断点续传：服务器接受了 Range 请求，追加写。
    Append,
    /// 本地文件已经完整——服务器对"从已有长度开始"的 Range 请求回了
    /// 416（Range Not Satisfiable）。跳过下载，直接进入校验。
    AlreadyComplete,
    /// 归档里确实没有这个分区（标的当时尚未上线等）。终态。
    NotInArchive,
    /// 分区所在月份还没到归档发布的时间（monthly 包要到下月初才生成）。
    /// 不是终态——应记为可重试的失败，而不是 `NotInArchive`。
    NotYetPublished,
    /// 其它任何状态码，视为错误。
    Error(String),
}

/// 归档月度包发布的时间规律：当月与上月的包可能还没生成。
///
/// 用来把"这个月份的 404 是不是因为包还没发布"与"归档里真的没有这个
/// 分区"区分开——前者应该重试，后者是终态。
fn previous_month(today: NaiveDate) -> (i32, u32) {
    let y = today.year();
    let m = today.month();
    if m == 1 { (y - 1, 12) } else { (y, m - 1) }
}

/// 把 HTTP 响应状态翻译成下载器该采取的动作。
///
/// 纯函数，不做任何 IO——方便穷举所有状态码组合做表驱动测试。
pub fn classify_response(
    status: reqwest::StatusCode,
    existing_len: u64,
    key: &PartitionKey,
    today: NaiveDate,
) -> RespAction {
    match status {
        reqwest::StatusCode::OK => RespAction::Fresh,
        reqwest::StatusCode::PARTIAL_CONTENT if existing_len > 0 => RespAction::Append,
        reqwest::StatusCode::RANGE_NOT_SATISFIABLE if existing_len > 0 => {
            RespAction::AlreadyComplete
        }
        reqwest::StatusCode::NOT_FOUND => {
            let (py, pm) = previous_month(today);
            if (key.year, key.month) >= (py, pm) {
                RespAction::NotYetPublished
            } else {
                RespAction::NotInArchive
            }
        }
        other => RespAction::Error(format!("HTTP {other}")),
    }
}

/// 从 `Content-Range: bytes a-b/TOTAL` 里取总长度。
///
/// `TOTAL` 为 `*` 时代表服务器不知道总长，返回 `None`。
fn parse_content_range_total(value: &str) -> Option<u64> {
    let total = value.rsplit('/').next()?;
    if total == "*" {
        None
    } else {
        total.parse().ok()
    }
}

/// 在 blocking 线程池里跑一段同步代码，把进度转发给 `progress.on_stage`。
///
/// `f` 收到的 `report` 回调：调用一次就把 `(done, total)` 发给异步侧触发
/// `on_stage`，返回值是"是否应继续"——`cancel` 一旦被置位就恒为 `false`，
/// `f` 应据此尽快返回一个文案含"取消"的错误。
///
/// 用 `mpsc` + `select!` 转发，而不是 `block_in_place`：本项目部分场景跑在
/// `current_thread` runtime 上，`block_in_place` 在其下会直接 panic。
async fn run_blocking_with_progress<T, F>(
    key: &PartitionKey,
    stage: Stage,
    progress: &mut dyn DownloadProgress,
    cancel: &CancelToken,
    f: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut dyn FnMut(u64, Option<u64>) -> bool) -> Result<T> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Option<u64>)>();
    let cancel = cancel.clone();

    let mut handle = tokio::task::spawn_blocking(move || {
        let mut report = move |done: u64, total: Option<u64>| -> bool {
            let _ = tx.send((done, total));
            !cancel.is_cancelled()
        };
        f(&mut report)
    });

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some((done, total)) => progress.on_stage(key, stage, done, total),
                    None => break,
                }
            }
            res = &mut handle => {
                while let Ok((done, total)) = rx.try_recv() {
                    progress.on_stage(key, stage, done, total);
                }
                return res.context("blocking 任务 panic")?;
            }
        }
    }

    // channel 已关闭（blocking 闭包已跑完 report），handle 应该马上就能拿到。
    while let Ok((done, total)) = rx.try_recv() {
        progress.on_stage(key, stage, done, total);
    }
    handle.await.context("blocking 任务 panic")?
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
    cancel: &CancelToken,
) -> Result<(DownloadOutcome, PartitionEntry)> {
    if cancel.is_cancelled() {
        bail!("已取消：{key}");
    }

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
    let action = classify_response(status, existing, key, Utc::now().date_naive());

    match action {
        RespAction::NotInArchive => {
            // 归档里没有这个分区。这是终态，不是错误——标的可能当时还没上线。
            // 记下请求的 URL：万一将来这个判断本身出错（例如又漏拼了一层
            // 路径），台账里留着的 URL 能让人一眼看出问题，而不必凭空猜测。
            progress.on_absent(key);
            return Ok((
                DownloadOutcome::NotInArchive,
                PartitionEntry {
                    status: PartitionStatus::NotInArchive,
                    source_url: Some(url),
                    ..PartitionEntry::absent()
                },
            ));
        }
        RespAction::NotYetPublished => {
            // 不是终态：不能标记 NotInArchive，否则会永久跳过这个分区。
            // 让它冒泡成 Err，调用方会记为 Failed（可重试）。
            bail!("归档尚未发布：{key}，月度包要到下月初才生成");
        }
        RespAction::Error(msg) => {
            bail!("{msg} 下载 {url}");
        }
        RespAction::AlreadyComplete => {
            // 本地文件已经完整（对已有字节的 Range 请求收到 416）。跳过
            // 下载，直接进入下面的 sha256 校验；校验不符时会删除文件并
            // 报错，下次重跑就是全新下载。
            progress.on_stage(key, Stage::Downloading, existing, Some(existing));
        }
        RespAction::Fresh | RespAction::Append => {
            let append = matches!(action, RespAction::Append);
            let total = if append {
                resp.headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range_total)
            } else {
                resp.content_length()
            };
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
            let mut done = if append { existing } else { 0 };
            while let Some(chunk) = stream.next().await {
                if cancel.is_cancelled() {
                    let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
                    bail!("已取消：{key}");
                }
                let chunk = chunk.context("下载流中断")?;
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
                done += chunk.len() as u64;
                progress.on_stage(key, Stage::Downloading, done, total);
            }
            tokio::io::AsyncWriteExt::flush(&mut file).await?;
        }
    }

    let zip_bytes = tokio::fs::metadata(&zip_path).await?.len();
    let dl_secs = started.elapsed().as_secs();
    progress.on_downloaded(key, zip_bytes, dl_secs);

    // ---- 2. 校验 sha256 ----
    let sha = {
        let zip_path_for_task = zip_path.clone();
        run_blocking_with_progress(key, Stage::Verifying, progress, cancel, move |report| {
            let mut adapt = |done: u64, total: u64| report(done, Some(total));
            sha256_file_with_progress(&zip_path_for_task, &mut adapt)
        })
        .await?
    };
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
    let csv_path = {
        let zip_path_for_task = zip_path.clone();
        let csv_dir_for_task = csv_dir.clone();
        let result =
            run_blocking_with_progress(key, Stage::Extracting, progress, cancel, move |report| {
                let mut adapt = |done: u64, total: u64| report(done, Some(total));
                extract_single_csv_with_progress(&zip_path_for_task, &csv_dir_for_task, &mut adapt)
            })
            .await;
        match result {
            Ok(p) => p,
            Err(e) => {
                if cancel.is_cancelled() {
                    let _ = tokio::fs::remove_dir_all(&csv_dir).await;
                }
                return Err(e);
            }
        }
    };

    // ---- 4. 转 Parquet ----
    let parquet_path = layout.parquet(key);
    if let Some(parent) = parquet_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let stats = {
        let csv_path_for_task = csv_path.clone();
        let parquet_path_for_task = parquet_path.clone();
        let kind = key.kind;
        let result =
            run_blocking_with_progress(key, Stage::Converting, progress, cancel, move |report| {
                let mut adapt = |done: u64, total: u64| report(done, Some(total));
                convert_csv_to_parquet_with_progress(
                    kind,
                    &csv_path_for_task,
                    &parquet_path_for_task,
                    &mut adapt,
                )
            })
            .await;
        match result {
            Ok(s) => s,
            Err(e) => {
                if cancel.is_cancelled() {
                    let _ = tokio::fs::remove_dir_all(&csv_dir).await;
                }
                return Err(e);
            }
        }
    };
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
#[cfg(test)]
fn sha256_file(path: &Path) -> Result<String> {
    sha256_file_with_progress(path, &mut |_, _| true)
}

/// 计算文件 sha256，每读约 8MB 汇报一次 `(done, total)`。
///
/// 回调返回 `false` 表示取消：立即返回文案含"取消"的错误，不删除
/// `path`（保留已下载的 ZIP 以便续传）。
fn sha256_file_with_progress(
    path: &Path,
    on_progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<String> {
    const REPORT_EVERY: u64 = 8 * 1024 * 1024;

    let mut file =
        std::fs::File::open(path).with_context(|| format!("打开文件失败: {}", path.display()))?;
    let total = file
        .metadata()
        .with_context(|| format!("读取文件元数据失败: {}", path.display()))?
        .len();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    let mut since_last = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        since_last += n as u64;
        if since_last >= REPORT_EVERY {
            since_last = 0;
            if !on_progress(done, total) {
                bail!("已取消：sha256 校验 {}", path.display());
            }
        }
    }
    if !on_progress(done, total) {
        bail!("已取消：sha256 校验 {}", path.display());
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
///
/// 带进度回调的解压：每写约 1MB 汇报一次 `(done, total)`，`total` 取自 ZIP
/// 条目的（未压缩）大小。
///
/// 回调返回 `false` 表示取消：删除已写出的部分 CSV 并返回文案含"取消"的
/// 错误。
fn extract_single_csv_with_progress(
    zip_path: &Path,
    out_dir: &Path,
    on_progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<PathBuf> {
    const REPORT_EVERY: u64 = 1024 * 1024;

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
        let total = entry.size();
        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("创建 CSV 失败: {}", out_path.display()))?;
        let mut buf = vec![0u8; 1024 * 1024];
        let mut done = 0u64;
        let mut since_last = 0u64;
        loop {
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done += n as u64;
            since_last += n as u64;
            if since_last >= REPORT_EVERY {
                since_last = 0;
                if !on_progress(done, total) {
                    drop(out);
                    let _ = std::fs::remove_file(&out_path);
                    bail!("已取消：解压 {}", zip_path.display());
                }
            }
        }
        out.sync_all()?;
        if !on_progress(done, total) {
            drop(out);
            let _ = std::fs::remove_file(&out_path);
            bail!("已取消：解压 {}", zip_path.display());
        }
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

    /// `today` 固定为 2026-09-26，便于表驱动测试月份边界。
    fn today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 26).unwrap()
    }

    /// `classify_response` 覆盖每个真正影响下载器行为的状态码组合：
    /// 200/206/416（各分有无本地文件）、404（老月份/上月/当月）、其它状态码。
    #[test]
    fn classify_response_table() {
        let k = |month: u32| key(DatasetKind::AggTrades, month);

        // 200：总是全新下载，不管本地是否已有文件。
        assert_eq!(
            classify_response(reqwest::StatusCode::OK, 0, &k(1), today()),
            RespAction::Fresh
        );
        assert_eq!(
            classify_response(reqwest::StatusCode::OK, 100, &k(1), today()),
            RespAction::Fresh
        );

        // 206：只有本地已有部分内容时才是续传，否则视为异常状态。
        assert_eq!(
            classify_response(reqwest::StatusCode::PARTIAL_CONTENT, 100, &k(1), today()),
            RespAction::Append
        );
        assert!(matches!(
            classify_response(reqwest::StatusCode::PARTIAL_CONTENT, 0, &k(1), today()),
            RespAction::Error(_)
        ));

        // 416：本地已有文件时代表"已经完整"，没有文件时是异常状态。
        assert_eq!(
            classify_response(
                reqwest::StatusCode::RANGE_NOT_SATISFIABLE,
                100,
                &k(1),
                today()
            ),
            RespAction::AlreadyComplete
        );
        assert!(matches!(
            classify_response(
                reqwest::StatusCode::RANGE_NOT_SATISFIABLE,
                0,
                &k(1),
                today()
            ),
            RespAction::Error(_)
        ));

        // 404 老月份：归档确实没有这个分区，终态。
        assert_eq!(
            classify_response(reqwest::StatusCode::NOT_FOUND, 0, &k(1), today()),
            RespAction::NotInArchive
        );

        // 404 上个月（今天是 2026-09-26，上月是 2026-08）：可能只是还没发布。
        assert_eq!(
            classify_response(reqwest::StatusCode::NOT_FOUND, 0, &k(8), today()),
            RespAction::NotYetPublished
        );

        // 404 当月：肯定还没发布。
        assert_eq!(
            classify_response(reqwest::StatusCode::NOT_FOUND, 0, &k(9), today()),
            RespAction::NotYetPublished
        );

        // 500：其它任何状态码都视为错误。
        assert!(matches!(
            classify_response(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                0,
                &k(1),
                today()
            ),
            RespAction::Error(_)
        ));
    }

    /// 跨年边界：今天是 1 月，上个月应该是去年 12 月，而不是月份 0。
    #[test]
    fn classify_response_handles_year_boundary_for_previous_month() {
        let jan = chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let dec_key = PartitionKey {
            kind: DatasetKind::AggTrades,
            symbol: "ETHUSDC".into(),
            year: 2025,
            month: 12,
        };
        assert_eq!(
            classify_response(reqwest::StatusCode::NOT_FOUND, 0, &dec_key, jan),
            RespAction::NotYetPublished
        );
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

    /// `clone()` 出的副本与原件共享同一个取消标记。
    #[test]
    fn cancel_token_clone_shares_state() {
        let original = CancelToken::default();
        let clone = original.clone();
        assert!(!original.is_cancelled());

        clone.cancel();

        assert!(
            original.is_cancelled(),
            "clone 上取消后，原件也应观察到已取消"
        );
    }

    #[test]
    fn content_range_total_parses_or_is_none_for_star() {
        assert_eq!(parse_content_range_total("bytes 0-99/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 100-199/*"), None);
        assert_eq!(parse_content_range_total("garbage"), None);
    }

    /// 用于测试的进度收集器：只记录 `on_stage`，其它回调空实现。
    struct StageCollector(Vec<(u64, Option<u64>)>);

    impl DownloadProgress for StageCollector {
        fn on_start(&mut self, _: &PartitionKey, _: &str) {}
        fn on_downloaded(&mut self, _: &PartitionKey, _: u64, _: u64) {}
        fn on_converted(&mut self, _: &PartitionKey, _: u64, _: u64) {}
        fn on_skip(&mut self, _: &PartitionKey) {}
        fn on_absent(&mut self, _: &PartitionKey) {}
        fn on_error(&mut self, _: &PartitionKey, _: &str) {}
        fn on_stage(&mut self, _: &PartitionKey, _: Stage, done: u64, total: Option<u64>) {
            self.0.push((done, total));
        }
    }

    /// 阻塞闭包发 3 条进度，异步侧应按序收到 3 条，并拿到闭包的返回值。
    #[tokio::test]
    async fn run_blocking_with_progress_forwards_messages_in_order() {
        let k = key(DatasetKind::AggTrades, 1);
        let mut collector = StageCollector(Vec::new());
        let cancel = CancelToken::default();

        let result: Result<i32> =
            run_blocking_with_progress(&k, Stage::Converting, &mut collector, &cancel, |report| {
                assert!(report(1, Some(10)));
                assert!(report(2, Some(10)));
                assert!(report(3, Some(10)));
                Ok(42)
            })
            .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(
            collector.0,
            vec![(1, Some(10)), (2, Some(10)), (3, Some(10))],
            "进度必须按发出顺序到达"
        );
    }
}
