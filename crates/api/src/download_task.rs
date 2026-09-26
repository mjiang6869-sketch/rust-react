//! 后台下载任务。
//!
//! # 与 CLI 的 `download` 子命令共用下载器
//!
//! 这里只做编排与进度上报——真正的下载、校验、转换逻辑在
//! `data::download::fetch_partition`，与 CLI 完全一致。
//!
//! # 归档索引与裁剪
//!
//! 下载前先经 [`AppState::archive_months`]（带缓存）取每个 (数据集, 交易对)
//! 组合的真实归档范围，交给 [`data::plan_work`] 裁剪请求区间——避免对超出
//! 归档范围的月份逐个发起注定 404 的请求，也避免把这些请求写进台账。
//!
//! # 进度上报与节流
//!
//! 进度经 [`DownloadJobGuard`] 落到共享快照，再经 [`AppState::broadcast`]
//! 推给 WebSocket。节流规则：阶段切换、分区开始/结束、任务结束都立即推；
//! 同一阶段内最多每 500ms 推一次——避免下载大文件时把 WebSocket 打爆。
//!
//! # 取消
//!
//! [`DownloadJobGuard::cancel_token`] 与 `data::fetch_partition` 共享同一个
//! 取消令牌。取消发生时**不写台账**——这是刻意的：取消是用户主动中断，
//! 半途而废的分区不应该被记成"失败"占用重试计数，也不应该被误记成任何
//! 终态。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use data::{
    ArchiveMonths, DEFAULT_ARCHIVE_BASE, DatasetKind, DownloadOutcome, DownloadProgress, Layout,
    Manifest, PartitionEntry, PartitionKey, PartitionStatus, Stage, fetch_partition, plan_work,
};

use crate::state::{
    AppState, DownloadJobCurrentSnapshot, DownloadJobGuard, DownloadJobPlanSnapshot,
    ProgressMessage,
};

/// 同一阶段内广播节流的最小间隔。
const STAGE_BROADCAST_INTERVAL: Duration = Duration::from_millis(500);
/// 归档索引查询之间的固定间隔，避免对 S3 无间隔连续请求。
const INDEX_QUERY_INTERVAL: Duration = Duration::from_millis(200);

/// 执行下载任务，直到完成、被取消，或遇到任务级错误。
///
/// 消耗 `guard`——任务结束时会调用 [`DownloadJobGuard::finish`] 并广播一次
/// 最终快照，调用方不需要（也不应该）再对这个 guard 做任何事。
pub async fn run(
    state: Arc<AppState>,
    guard: DownloadJobGuard,
    symbols: Vec<String>,
    kinds: Vec<DatasetKind>,
    from: (i32, u32),
    to: (i32, u32),
) {
    let outcome = run_inner(&state, &guard, &symbols, &kinds, from, to).await;
    match outcome {
        Ok(true) => finish_and_broadcast(&state, guard, "cancelled"),
        Ok(false) => finish_and_broadcast(&state, guard, "finished"),
        Err(e) => {
            guard.set_last_error(format!("{e:#}"));
            finish_and_broadcast(&state, guard, "failed");
        }
    }
}

fn finish_and_broadcast(state: &AppState, guard: DownloadJobGuard, terminal_state: &str) {
    let job = guard.finish(terminal_state);
    state.broadcast(ProgressMessage::DownloadStatus { job: Box::new(job) });
}

/// 返回 `Ok(true)` 表示任务被取消，`Ok(false)` 表示正常跑完（含"全部分区
/// 早已完成，无需下载"的情形）。`Err` 是任务级错误（例如台账无法加载），
/// 不区分具体分区——分区级错误在循环内部吸收，不会让整个任务失败。
async fn run_inner(
    state: &AppState,
    guard: &DownloadJobGuard,
    symbols: &[String],
    kinds: &[DatasetKind],
    from: (i32, u32),
    to: (i32, u32),
) -> Result<bool> {
    let layout = Layout::new(&state.data_root);
    let mut manifest = Manifest::load(&layout.manifest_path())?;
    let cancel = guard.cancel_token();

    // ---- 归档索引：串行查询，间隔 200ms ----
    let mut index_cache: HashMap<(DatasetKind, String), Option<ArchiveMonths>> = HashMap::new();
    let mut first = true;
    for &kind in kinds {
        for symbol in symbols {
            if cancel.is_cancelled() {
                return Ok(true);
            }
            if !first {
                tokio::time::sleep(INDEX_QUERY_INTERVAL).await;
            }
            first = false;
            let months = match state.archive_months(kind, symbol).await {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!(
                        "列举 {symbol} 的 {kind:?} 归档失败，本次计划将退化为无索引裁剪：{e}"
                    );
                    None
                }
            };
            index_cache.insert((kind, symbol.clone()), months);
        }
    }

    let index_fn = |kind: DatasetKind, symbol: &str| -> Option<ArchiveMonths> {
        index_cache
            .get(&(kind, symbol.to_string()))
            .cloned()
            .flatten()
    };

    let today = Utc::now().date_naive();
    let plan = plan_work(&manifest, kinds, symbols, from, to, &index_fn, today);

    guard.set_plan(DownloadJobPlanSnapshot {
        total: plan.work.len(),
        clipped: plan.clipped,
        index_available: plan.index_available,
    });
    state.broadcast(ProgressMessage::DownloadStatus {
        job: Box::new(guard.snapshot()),
    });

    if plan.work.is_empty() {
        return Ok(false);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent("rust-crypto-research/0.1")
        .build()
        .context("构造 HTTP 客户端失败")?;

    // 串行下载：并发会让进度上报乱序，而磁盘 IO 与 Parquet 转换本身是
    // 瓶颈（转换是 CPU 密集操作）。串行也让取消语义更简单——取消只需要让
    // "当前正在处理的那一个"尽快退出。
    for key in &plan.work {
        if cancel.is_cancelled() {
            return Ok(true);
        }

        let mut progress = JobProgress::new(state, guard);
        let result = fetch_partition(
            &client,
            &layout,
            key,
            DEFAULT_ARCHIVE_BASE,
            &mut progress,
            &cancel,
        )
        .await;

        match result {
            Ok((outcome, entry)) => {
                let suspicious = matches!(entry.status, PartitionStatus::Suspicious { .. });
                manifest.record_and_save(&layout.manifest_path(), key.clone(), entry)?;
                match outcome {
                    DownloadOutcome::Done { .. } => guard.record_completed(),
                    DownloadOutcome::NotInArchive => guard.record_not_in_archive(),
                    DownloadOutcome::Skipped => guard.record_done(),
                }
                if suspicious {
                    tracing::warn!("分区 {key} 行数与期望不符，已标记待查");
                }
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    // 取消导致的失败：不写台账，也不计入 failures——见模块文档。
                    return Ok(true);
                }
                let attempts = match manifest.entry(key).map(|x| &x.status) {
                    Some(PartitionStatus::Failed { attempts, .. }) => attempts + 1,
                    _ => 1,
                };
                let failed_entry = PartitionEntry {
                    status: PartitionStatus::Failed {
                        error: format!("{e:#}"),
                        attempts,
                    },
                    ..PartitionEntry::absent()
                };
                manifest.record_and_save(&layout.manifest_path(), key.clone(), failed_entry)?;
                guard.record_failure(key.to_string(), format!("{e:#}"));
                tracing::warn!("分区 {key} 下载失败：{e:#}");
            }
        }

        // 分区结束：立即推一次，不受节流限制。
        state.broadcast(ProgressMessage::DownloadStatus {
            job: Box::new(guard.snapshot()),
        });
    }

    Ok(false)
}

/// 把 `fetch_partition` 的细粒度进度转成共享快照更新 + 节流广播。
struct JobProgress<'a> {
    state: &'a AppState,
    guard: &'a DownloadJobGuard,
    last_stage: Option<Stage>,
    stage_started_at: DateTime<Utc>,
    last_broadcast: Instant,
}

impl<'a> JobProgress<'a> {
    fn new(state: &'a AppState, guard: &'a DownloadJobGuard) -> Self {
        Self {
            state,
            guard,
            last_stage: None,
            stage_started_at: Utc::now(),
            // 减去节流间隔：让第一次调用（`on_start`）必然触发广播，而不是
            // 因为"距上次广播不足 500ms"被节流掉——那样界面就看不到分区
            // 开始的那一刻。用 `checked_sub` 兜底极端情况（进程刚启动、
            // 单调时钟原点极早），避免理论上的下溢 panic。
            last_broadcast: Instant::now()
                .checked_sub(STAGE_BROADCAST_INTERVAL)
                .unwrap_or_else(Instant::now),
        }
    }

    fn set_current(&self, key: &PartitionKey, stage: Stage, done: u64, total: Option<u64>) {
        self.guard.set_current(Some(DownloadJobCurrentSnapshot {
            symbol: key.symbol.clone(),
            kind: key.kind.api_name().to_string(),
            month: format!("{}-{:02}", key.year, key.month),
            stage: stage.tag().to_string(),
            stage_label: stage.label().to_string(),
            stage_done: done,
            stage_total: total,
            stage_started_at: self.stage_started_at,
        }));
    }

    fn broadcast_now(&mut self) {
        self.last_broadcast = Instant::now();
        self.state.broadcast(ProgressMessage::DownloadStatus {
            job: Box::new(self.guard.snapshot()),
        });
    }
}

impl DownloadProgress for JobProgress<'_> {
    fn on_start(&mut self, key: &PartitionKey, _url: &str) {
        // 还没有真正的阶段进度——预设为"下载"，紧接着的 `on_stage` 调用
        // 会带来真实的 done/total。分区开始，立即推一次。
        self.last_stage = Some(Stage::Downloading);
        self.stage_started_at = Utc::now();
        self.set_current(key, Stage::Downloading, 0, None);
        self.broadcast_now();
    }

    fn on_downloaded(&mut self, _key: &PartitionKey, _bytes: u64, _secs: u64) {}
    fn on_converted(&mut self, _key: &PartitionKey, _rows: u64, _secs: u64) {}
    fn on_skip(&mut self, _key: &PartitionKey) {}
    fn on_absent(&mut self, _key: &PartitionKey) {}
    fn on_error(&mut self, _key: &PartitionKey, _err: &str) {}

    fn on_stage(&mut self, key: &PartitionKey, stage: Stage, done: u64, total: Option<u64>) {
        let switched = self.last_stage != Some(stage);
        if switched {
            self.last_stage = Some(stage);
            self.stage_started_at = Utc::now();
        }
        self.set_current(key, stage, done, total);

        if switched || self.last_broadcast.elapsed() >= STAGE_BROADCAST_INTERVAL {
            self.broadcast_now();
        }
    }
}
