//! 共享应用状态。
//!
//! # 为什么引擎与数据库是两把独立的锁
//!
//! 引擎的状态变更（喂行情、判定成交）与数据库的读写（查订单历史、写回测记录）
//! 是两种不同粒度的操作。合成一把锁会让"查询订单列表"阻塞"行情处理"——在
//! 做市这种对延迟敏感的场景里不可接受。
//!
//! 代价是需要留意两者的一致性：引擎状态变更后要显式落库。这个责任落在
//! `engine` 层，而不是在这里自动做——自动同步会让"什么时候写了库"变得隐式。
//!
//! # 下载任务状态为什么用 `std::sync::Mutex`
//!
//! [`DownloadJobs`] 内部的临界区都很短（读/写一份快照结构体），不涉及任何
//! `.await`。用 `std::sync::Mutex` 而不是 `tokio::sync::Mutex`：前者没有异步
//! 开销，且能在 `Drop` 里同步使用（`tokio::sync::Mutex` 的 `lock()` 是异步的，
//! `Drop` 里没有 executor 可以 `.await`）。**规则：锁绝不跨 `.await` 持有**——
//! 每个持锁的方法体都只做字段读写，拿到需要的值后立刻释放。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use chrono::{DateTime, Utc};
use data::DatasetKind;
use domain::ServiceMode;
use engine::{EngineConfig, PaperEngine};
use exchange::{BinanceClient, MarketStreams, PRODUCTION_URL};
use rusqlite::Connection;
use tokio::sync::Mutex;

/// 后台任务推送给界面的进度消息。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressMessage {
    /// 下载任务的完整状态快照。取代旧的 `Download` / `DownloadDone` 两个
    /// 变体——界面靠一份完整快照就能重建整个进度面板，不需要拼接增量。
    ///
    /// `Box`：`DownloadJobSnapshot` 比 `Backtest` 变体大出好几倍（失败列表、
    /// 当前分区等字段），不装箱会让整个枚举按最大变体分配，即便绝大多数
    /// 消息其实是体积小得多的回测进度。
    DownloadStatus { job: Box<DownloadJobSnapshot> },
    /// 回测进度。
    Backtest {
        symbol: String,
        model: String,
        done: usize,
        total: usize,
    },
}

// ---------------------------------------------------------------------------
// 下载任务状态
// ---------------------------------------------------------------------------

/// 下载请求的回显（供快照展示，不做二次校验）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloadJobRequestSnapshot {
    pub symbols: Vec<String>,
    pub kinds: Vec<String>,
    pub from: String,
    pub to: String,
}

/// 裁剪后的下载计划摘要。
#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloadJobPlanSnapshot {
    pub total: usize,
    pub clipped: Vec<String>,
    pub index_available: bool,
}

/// 正在处理的分区。
#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloadJobCurrentSnapshot {
    pub symbol: String,
    pub kind: String,
    pub month: String,
    pub stage: String,
    pub stage_label: String,
    pub stage_done: u64,
    pub stage_total: Option<u64>,
    pub stage_started_at: DateTime<Utc>,
}

/// 一个分区的失败记录。
#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloadJobFailureSnapshot {
    pub partition: String,
    pub error: String,
}

/// `failures` 列表的上限。任务可能有几百个失败分区（例如整段区间归档都
/// 缺失），无限增长的列表既没有排查价值又浪费带宽。
const MAX_FAILURES: usize = 50;

/// 下载任务的完整状态快照。这是 WebSocket 推送与 REST 查询共用的唯一形状。
#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloadJobSnapshot {
    /// `"idle"` | `"running"` | `"finished"` | `"cancelled"` | `"failed"`。
    pub state: String,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub request: Option<DownloadJobRequestSnapshot>,
    pub plan: Option<DownloadJobPlanSnapshot>,
    pub done: usize,
    pub completed: usize,
    pub not_in_archive: usize,
    pub failed: usize,
    pub current: Option<DownloadJobCurrentSnapshot>,
    pub failures: Vec<DownloadJobFailureSnapshot>,
    pub last_error: Option<String>,
}

impl Default for DownloadJobSnapshot {
    fn default() -> Self {
        Self {
            state: "idle".to_string(),
            started_at: None,
            finished_at: None,
            request: None,
            plan: None,
            done: 0,
            completed: 0,
            not_in_archive: 0,
            failed: 0,
            current: None,
            failures: Vec::new(),
            last_error: None,
        }
    }
}

struct DownloadJobsInner {
    snapshot: DownloadJobSnapshot,
    cancel: Option<data::CancelToken>,
}

/// 从中毒的锁里也能拿到内容——下载任务的临界区从不做可能 panic 的复杂计算，
/// 但持锁方（例如 `Drop`）绝不能因为别处的 panic 而跟着 panic 或死锁。
fn lock_inner(inner: &StdMutex<DownloadJobsInner>) -> MutexGuard<'_, DownloadJobsInner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// 单任务下载状态机：同一时间只允许一个下载任务在跑。
///
/// 内部只有一把 `std::sync::Mutex`，且所有持锁方法都只做字段读写——从不
/// `.await`。这样才能在 [`DownloadJobGuard::drop`] 里同步上锁。
pub struct DownloadJobs {
    inner: Arc<StdMutex<DownloadJobsInner>>,
}

impl Default for DownloadJobs {
    fn default() -> Self {
        Self {
            inner: Arc::new(StdMutex::new(DownloadJobsInner {
                snapshot: DownloadJobSnapshot::default(),
                cancel: None,
            })),
        }
    }
}

impl DownloadJobs {
    /// 尝试开始一个新任务。已有任务在跑时返回 `Err`，且**不会**修改任何状态
    /// ——调用方应据此返回 409，且不能因为这次失败的尝试而误清掉真正在跑的
    /// 那个任务的进度。
    pub fn try_start(
        &self,
        request: DownloadJobRequestSnapshot,
    ) -> Result<DownloadJobGuard, String> {
        let mut guard = lock_inner(&self.inner);
        if guard.snapshot.state == "running" {
            return Err("已有下载任务在进行".to_string());
        }
        let cancel = data::CancelToken::default();
        guard.snapshot = DownloadJobSnapshot {
            state: "running".to_string(),
            started_at: Some(Utc::now()),
            request: Some(request),
            ..DownloadJobSnapshot::default()
        };
        guard.cancel = Some(cancel.clone());
        drop(guard);
        Ok(DownloadJobGuard {
            inner: self.inner.clone(),
            cancel,
            finished: false,
        })
    }

    /// 当前快照。没有任务时是 `state == "idle"` 的默认值。
    pub fn snapshot(&self) -> DownloadJobSnapshot {
        lock_inner(&self.inner).snapshot.clone()
    }

    /// 取消当前任务。没有任务在跑时返回 `false`，不做任何事。
    pub fn cancel(&self) -> bool {
        let guard = lock_inner(&self.inner);
        if guard.snapshot.state != "running" {
            return false;
        }
        match &guard.cancel {
            Some(c) => {
                c.cancel();
                true
            }
            None => false,
        }
    }
}

/// 一次下载任务的 RAII 句柄。
///
/// # 为什么需要 `Drop` 兜底
///
/// 下载任务跑在一个独立的 `tokio::spawn` 里。如果任务因为未预料的 panic
/// 提前结束（例如某个 `unwrap` 炸了），**必须**有人把状态从 `running` 改回
/// 终态，否则 [`DownloadJobs::try_start`] 会永久拒绝所有后续请求，界面上
/// 也会一直显示"进行中"的假象。`Drop` 检查"这个 guard 是否已经显式
/// `finish()` 过"，没有就强制标记为 `failed`。
pub struct DownloadJobGuard {
    inner: Arc<StdMutex<DownloadJobsInner>>,
    cancel: data::CancelToken,
    finished: bool,
}

impl DownloadJobGuard {
    /// 与当前任务共享状态的取消令牌。克隆出的副本与原件共享底层标记。
    pub fn cancel_token(&self) -> data::CancelToken {
        self.cancel.clone()
    }

    fn with_snapshot<F: FnOnce(&mut DownloadJobSnapshot)>(&self, f: F) {
        let mut guard = lock_inner(&self.inner);
        f(&mut guard.snapshot);
    }

    pub fn set_plan(&self, plan: DownloadJobPlanSnapshot) {
        self.with_snapshot(|s| s.plan = Some(plan));
    }

    pub fn set_current(&self, current: Option<DownloadJobCurrentSnapshot>) {
        self.with_snapshot(|s| s.current = current);
    }

    /// 记一个不算成功/失败/归档缺失的完成（目前只有 `Skipped` 会走这里）。
    pub fn record_done(&self) {
        self.with_snapshot(|s| s.done += 1);
    }

    pub fn record_completed(&self) {
        self.with_snapshot(|s| {
            s.completed += 1;
            s.done += 1;
        });
    }

    pub fn record_not_in_archive(&self) {
        self.with_snapshot(|s| {
            s.not_in_archive += 1;
            s.done += 1;
        });
    }

    pub fn record_failure(&self, partition: String, error: String) {
        self.with_snapshot(|s| {
            s.failed += 1;
            s.done += 1;
            if s.failures.len() >= MAX_FAILURES {
                s.failures.remove(0);
            }
            s.failures
                .push(DownloadJobFailureSnapshot { partition, error });
        });
    }

    pub fn set_last_error(&self, err: String) {
        self.with_snapshot(|s| s.last_error = Some(err));
    }

    /// 当前快照（不消耗 guard，任务运行中随时可查）。
    pub fn snapshot(&self) -> DownloadJobSnapshot {
        lock_inner(&self.inner).snapshot.clone()
    }

    /// 显式收尾。消耗 guard——收尾之后不应该再更新任何进度。
    ///
    /// 返回收尾后的快照，方便调用方立即广播一次"任务结束"消息，而不必
    /// 再单独查一次。
    pub fn finish(mut self, state: &str) -> DownloadJobSnapshot {
        self.finished = true;
        self.with_snapshot(|s| {
            s.state = state.to_string();
            s.finished_at = Some(Utc::now());
            s.current = None;
        });
        self.snapshot()
    }
}

/// 手写 `Debug`：`CancelToken`（`data` crate）没有派生 `Debug`，没法直接
/// `#[derive]`。只打印对排查有用的字段——`finished` 决定 `Drop` 会不会
/// 兜底改状态，这也是测试里 `unwrap_err()` 需要 `Debug` 的唯一原因。
impl std::fmt::Debug for DownloadJobGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadJobGuard")
            .field("finished", &self.finished)
            .finish()
    }
}

impl Drop for DownloadJobGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut guard = lock_inner(&self.inner);
        if guard.snapshot.state == "running" {
            guard.snapshot.state = "failed".to_string();
            guard.snapshot.finished_at = Some(Utc::now());
            guard.snapshot.current = None;
            guard.snapshot.last_error = Some("任务意外结束".to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// 归档列举缓存
// ---------------------------------------------------------------------------

/// 成功结果的缓存有效期。归档月份范围几乎不变，6 小时内不必重新列举。
const ARCHIVE_CACHE_SUCCESS_TTL: chrono::Duration = chrono::Duration::hours(6);
/// 失败结果的缓存有效期。网络抖动等短暂问题很快会恢复，但也不能让一次
/// 失败在几秒内被同一个请求打好几次——30 秒内复用同一个错误。
const ARCHIVE_CACHE_FAILURE_TTL: chrono::Duration = chrono::Duration::seconds(30);

#[derive(Clone)]
struct ArchiveCacheEntry {
    at: DateTime<Utc>,
    result: Result<data::ArchiveMonths, String>,
}

/// 判断一条缓存记录是否已过期。纯函数，不做任何 IO——方便直接单测两条
/// TTL（成功 6 小时 / 失败 30 秒）的边界。
fn is_cache_entry_stale(stored_at: DateTime<Utc>, now: DateTime<Utc>, success: bool) -> bool {
    let ttl = if success {
        ARCHIVE_CACHE_SUCCESS_TTL
    } else {
        ARCHIVE_CACHE_FAILURE_TTL
    };
    now - stored_at >= ttl
}

/// 应用共享状态。
pub struct AppState {
    /// 模拟盘引擎。
    pub engine: Arc<Mutex<PaperEngine>>,
    /// SQLite 连接（热状态）。
    ///
    /// 用 `Mutex` 而非连接池：SQLite 的 WAL 模式已支持一写多读，而我们的
    /// 写入频率很低（每次订单状态变更），连接池带来的复杂度不值得。
    pub db: Arc<Mutex<Connection>>,
    /// 数据根目录（Parquet 与台账所在）。
    pub data_root: PathBuf,
    /// 后台任务进度广播通道。
    pub progress_tx: tokio::sync::broadcast::Sender<ProgressMessage>,
    /// 幂等键缓存。
    ///
    /// 手动面板一定会被双击。没有它就会下出两张单。
    seen_idempotency_keys: Mutex<HashSet<String>>,
    /// 当前服务模式。
    mode: Mutex<ServiceMode>,
    /// 默认交易对。
    ///
    /// 单独存一份而不是每次锁引擎读 `snapshot().symbol`：那会与行情处理争锁，
    /// 而行情处理是做市链路里最不该被阻塞的一环。这个值在构造时就固定了。
    symbol: String,
    /// 公开行情客户端（无需凭据）。
    ///
    /// 只用于**补数据**：图表要 K 线、成交流要开屏的那几笔，而本地归档可能
    /// 还没下载完。它不是实时行情源——实时盘口与成交走 [`Self::market_streams`]，
    /// REST 轮询做实时更新既浪费配额又慢（事故就是这么来的）。
    ///
    /// 惰性构造：构造失败（网络配置异常）不应该让服务起不来，界面退化成
    /// 「暂无数据」而不是整个进程挂掉。
    market: std::sync::OnceLock<Option<BinanceClient>>,
    /// 上游限流冷却。
    ///
    /// **必须建在状态上而不是客户端内部**：封禁记在 IP 上，同一个进程里
    /// 只要有一个客户端撞到 418，所有请求方都得一起等。放在这里的话，
    /// 将来新增的行情客户端（或后台任务自己建的）可以 `adopt` 到同一份
    /// 冷却上，不会各撞一次。
    market_cooldown: exchange::Cooldown,
    /// 行情推送（盘口与成交流）。
    ///
    /// 与 REST 共用 `market_cooldown`：REST 撞到 418 期间推送不去握手，推送
    /// 握手被限流也会让 REST 一起停。构造时不连网，第一个订阅者到来时才连。
    ///
    /// `None` 表示行情 REST 被指到了非生产地址。推送只接生产网——测试网的
    /// 推送入口与生产不同，混用会让盘口与 K 线来自两个不同的市场。
    market_streams: Option<MarketStreams>,
    /// 下载任务状态机。单任务，见 [`DownloadJobs`]。
    downloads: DownloadJobs,
    /// 币安归档列举（S3）的结果缓存。key 是 `(数据集, 交易对)`。
    ///
    /// 用 `std::sync::Mutex`：临界区只是查表/写表，从不跨 `.await`。
    archive_cache: StdMutex<HashMap<(DatasetKind, String), ArchiveCacheEntry>>,
    /// 归档列举专用的 HTTP 客户端。与下载任务用的下载客户端分开：前者
    /// 只做小请求（列目录 XML），15 秒超时足够；下载客户端要扛几百 MB
    /// 的传输，用的是分钟级超时。两者共用同一个 `user_agent`。
    archive_client: reqwest::Client,
}

impl AppState {
    pub fn new(engine: PaperEngine, db: Connection, data_root: PathBuf) -> Arc<Self> {
        let (progress_tx, _) = tokio::sync::broadcast::channel(256);
        let symbol = engine.snapshot().symbol;
        let market_cooldown = exchange::Cooldown::new();
        let market_streams = market_rest_base()
            .eq(PRODUCTION_URL)
            .then(|| MarketStreams::production(market_cooldown.clone()));
        let archive_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("rust-crypto-research/0.1")
            .build()
            .expect("构造归档列举 HTTP 客户端失败");
        Arc::new(Self {
            engine: Arc::new(Mutex::new(engine)),
            db: Arc::new(Mutex::new(db)),
            data_root,
            symbol,
            progress_tx,
            seen_idempotency_keys: Mutex::new(HashSet::new()),
            mode: Mutex::new(ServiceMode::Paper),
            market: std::sync::OnceLock::new(),
            market_cooldown,
            market_streams,
            downloads: DownloadJobs::default(),
            archive_cache: StdMutex::new(HashMap::new()),
            archive_client,
        })
    }

    /// 从配置构造。
    pub fn from_config(config: EngineConfig, db: Connection, data_root: PathBuf) -> Arc<Self> {
        Self::new(PaperEngine::new(config), db, data_root)
    }

    pub fn schema_version(&self) -> i32 {
        store::CURRENT_VERSION
    }

    /// 默认交易对。
    pub fn symbol(&self) -> String {
        self.symbol.clone()
    }

    /// 公开行情客户端。首次调用时构造并缓存。
    ///
    /// base URL 可用 `RUST_CRYPTO_BINANCE_MARKET_URL` 覆盖，但只能指向
    /// 白名单内的域名——测试网与生产网的行情数据不同，误指会让图表显示的
    /// 价格与将要下单的价格不一致。
    pub fn market(&self) -> Option<&BinanceClient> {
        self.market
            .get_or_init(|| {
                let base = market_rest_base();
                // 注入共享冷却：这个客户端的 429/418 会被记在状态上，
                // 其它请求方（以及将来的 WebSocket 重连逻辑）都能看到。
                match BinanceClient::public_with_cooldown(&base, self.market_cooldown.clone()) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!("行情客户端构造失败，图表将无数据：{e}");
                        None
                    }
                }
            })
            .as_ref()
    }

    /// 行情上游的冷却状态。
    ///
    /// 给 API 层用：即便行情客户端还没构造出来，也要能回答「现在是不是
    /// 正在被限流」，否则界面会在封禁期间一直重试并显示无意义的错误。
    pub fn market_cooldown(&self) -> &exchange::Cooldown {
        &self.market_cooldown
    }

    /// 行情推送。`None` 的含义见字段文档。
    pub fn market_streams(&self) -> Option<&MarketStreams> {
        self.market_streams.as_ref()
    }

    /// 下载任务状态机。
    pub fn downloads(&self) -> &DownloadJobs {
        &self.downloads
    }

    /// 某数据集某标的的归档月份范围，带缓存。
    ///
    /// # 锁的用法
    ///
    /// 先在锁内查表，锁外做网络请求，最后再短暂上锁写回——**不会**跨
    /// `.await` 持锁。
    pub async fn archive_months(
        &self,
        kind: DatasetKind,
        symbol: &str,
    ) -> Result<data::ArchiveMonths, String> {
        let key = (kind, symbol.to_string());
        let now = Utc::now();

        {
            let cache = self.archive_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(&key) {
                let success = entry.result.is_ok();
                if !is_cache_entry_stale(entry.at, now, success) {
                    return entry.result.clone();
                }
            }
        }

        let result = data::fetch_archive_months(&self.archive_client, kind, symbol)
            .await
            .map_err(|e| format!("{e:#}"));

        {
            let mut cache = self.archive_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(
                key,
                ArchiveCacheEntry {
                    at: Utc::now(),
                    result: result.clone(),
                },
            );
        }

        result
    }

    pub async fn mode(&self) -> ServiceMode {
        *self.mode.lock().await
    }

    pub async fn set_mode(&self, mode: ServiceMode) {
        *self.mode.lock().await = mode;

        // 切到实盘时，把已启用的自动化做市关掉——见 AGENTS.md：不能因为
        // 模式切换让一个此前对着模拟盘校准的策略突然对着真实资金跑。
        //
        // 锁顺序：`mode` 锁在上一行已经完全释放（赋值语句结束时 guard 就
        // 被 drop 了），这里才去拿引擎锁——不会出现"持有 mode 锁等引擎锁"
        // 的路径。这与 `auto_maker::put` 里"持有引擎锁、再短暂拿一次 mode
        // 锁"是相反的方向，两个方向不会同时嵌套等待对方，所以不会死锁。
        //
        // 即便两个请求并发交错（一个在切模式，一个在开自动化做市），
        // 由 `tokio::sync::Mutex` 的公平排队保证：无论谁先拿到引擎锁，
        // 最终状态一定是一致的——自动化做市要么从未被打开，要么打开后
        // 立刻被这里关掉，不会出现"实盘 + 自动化做市开着"的组合。
        if mode.is_live() {
            self.engine.lock().await.disable_auto_maker();
        }
    }

    /// 该幂等键是否已处理过。
    pub async fn is_duplicate_submit(&self, key: &str) -> bool {
        self.seen_idempotency_keys.lock().await.contains(key)
    }

    /// 记住幂等键。
    ///
    /// 缓存有上限：无界增长会在长跑的服务里泄漏内存。超过上限时清空——
    /// 幂等键的有效期本来就很短（防双击），清空只会让极老的重复请求
    /// 有机会通过，而那种情况几乎不可能发生。
    pub async fn remember_submit(&self, key: String) {
        const MAX_KEYS: usize = 10_000;
        let mut set = self.seen_idempotency_keys.lock().await;
        if set.len() >= MAX_KEYS {
            set.clear();
        }
        set.insert(key);
    }

    /// 广播一条进度消息。
    pub fn broadcast(&self, msg: ProgressMessage) {
        // 没有订阅者时返回 Err，不是错误——界面可能还没打开。
        let _ = self.progress_tx.send(msg);
    }
}

/// 行情 REST 的 base URL。`RUST_CRYPTO_BINANCE_MARKET_URL` 可覆盖。
fn market_rest_base() -> String {
    std::env::var("RUST_CRYPTO_BINANCE_MARKET_URL").unwrap_or_else(|_| PRODUCTION_URL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{ContractKind, FeeSchedule, FeeSource, Instrument, Precision};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn instrument() -> Instrument {
        Instrument {
            symbol: "ETHUSDC".into(),
            kind: ContractKind::CryptoPerp,
            base_asset: "ETH".into(),
            quote_asset: "USDC".into(),
            margin_asset: "USDC".into(),
            settlement_asset: "USDC".into(),
            precision: Precision {
                tick_size: dec!(0.01),
                step_size: dec!(0.001),
                min_qty: dec!(0.001),
                min_notional: dec!(5),
            },
            maint_margin_pct: dec!(2.5),
            required_margin_pct: dec!(5),
            liquidation_fee: dec!(0.0125),
            fees: FeeSchedule {
                maker_rate: Decimal::ZERO,
                taker_rate: dec!(0.0005),
                source: FeeSource::PromotionalAssumed,
                observed_at: chrono::Utc::now(),
            },
        }
    }

    fn state() -> Arc<AppState> {
        let conn = Connection::open_in_memory().unwrap();
        store::configure(&conn).unwrap();
        store::migrate(&conn).unwrap();
        AppState::from_config(
            EngineConfig {
                instrument: instrument(),
                limits: domain::RiskLimits::default(),
                initial_equity: dec!(10000),
                assumed_latency_ms: 100,
                max_candles: 120,
                max_staleness_secs: 15,
            },
            conn,
            PathBuf::from("/tmp/rc-test"),
        )
    }

    fn sample_request() -> DownloadJobRequestSnapshot {
        DownloadJobRequestSnapshot {
            symbols: vec!["ETHUSDC".into()],
            kinds: vec!["klines".into()],
            from: "2026-01".into(),
            to: "2026-02".into(),
        }
    }

    #[tokio::test]
    async fn default_mode_is_paper() {
        let s = state();
        assert_eq!(s.mode().await, ServiceMode::Paper);
        assert!(!s.mode().await.allows_real_orders());
    }

    #[tokio::test]
    async fn mode_can_be_changed() {
        let s = state();
        s.set_mode(ServiceMode::Live).await;
        assert_eq!(s.mode().await, ServiceMode::Live);
    }

    /// 切到实盘必须自动关闭已启用的自动化做市——不能让一个对着模拟盘
    /// 校准的策略因为一次模式切换就突然对着真实资金跑。
    #[tokio::test]
    async fn set_mode_live_disables_auto_maker() {
        let s = state();
        s.engine
            .lock()
            .await
            .configure_auto_maker(true, None)
            .expect("默认参数应能开启");
        assert!(s.engine.lock().await.auto_maker().enabled, "开启应生效");

        s.set_mode(ServiceMode::Live).await;

        assert!(
            !s.engine.lock().await.auto_maker().enabled,
            "切到实盘后自动化做市必须被关闭"
        );
    }

    /// 幂等保护必须真的生效——手动面板双击是最常见的误操作。
    #[tokio::test]
    async fn idempotency_key_blocks_duplicate() {
        let s = state();
        assert!(!s.is_duplicate_submit("k1").await);
        s.remember_submit("k1".to_string()).await;
        assert!(s.is_duplicate_submit("k1").await, "重复的键必须被识别");
        assert!(!s.is_duplicate_submit("k2").await, "不同的键不受影响");
    }

    /// 幂等键缓存不能无界增长。
    #[tokio::test]
    async fn idempotency_cache_is_bounded() {
        let s = state();
        for i in 0..10_100 {
            s.remember_submit(format!("k{i}")).await;
        }
        // 清空后旧键应查不到，但服务不应崩溃或耗尽内存
        assert!(!s.is_duplicate_submit("k0").await);
        assert!(
            s.is_duplicate_submit("k10099").await,
            "最新的键应仍在缓存里"
        );
    }

    #[tokio::test]
    async fn schema_version_matches_store() {
        let s = state();
        assert_eq!(s.schema_version(), store::CURRENT_VERSION);
    }

    /// 没有订阅者时广播不应 panic——界面可能还没打开。
    #[tokio::test]
    async fn broadcast_without_subscribers_does_not_panic() {
        let s = state();
        s.broadcast(ProgressMessage::DownloadStatus {
            job: Box::new(DownloadJobSnapshot::default()),
        });
    }

    /// 冷却状态在状态层共享——即便行情客户端还没构造，也能回答
    /// 「现在是否被限流」。事故里界面在封禁期间一直重试就是这个信息缺失。
    #[tokio::test]
    async fn market_cooldown_is_shared_and_visible_before_client_exists() {
        let s = state();
        assert!(s.market_cooldown().remaining_ms().is_none(), "初始无冷却");
        s.market_cooldown().arm_ms(60_000);
        assert!(
            s.market_cooldown().is_cooling_down(),
            "状态层必须能看到冷却，否则界面无法退避"
        );
    }

    #[tokio::test]
    async fn progress_messages_are_serializable() {
        let msg = ProgressMessage::DownloadStatus {
            job: Box::new(DownloadJobSnapshot::default()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"download_status\""), "{json}");
    }

    // ---- DownloadJobs / DownloadJobGuard ----

    /// 已有任务在跑时，第二次 `try_start` 必须失败，且不能影响第一个任务。
    #[test]
    fn try_start_blocks_concurrent_second_task() {
        let jobs = DownloadJobs::default();
        let guard = jobs.try_start(sample_request()).expect("第一次应成功");
        let err = jobs.try_start(sample_request()).unwrap_err();
        assert!(err.contains("已有下载任务在进行"), "{err}");
        assert_eq!(jobs.snapshot().state, "running", "第一个任务不应被影响");
        drop(guard);
    }

    /// guard 被 drop 而没有显式 `finish()`（模拟 panic 或提前返回）时，
    /// 状态必须变成 `failed`，不能停在 `running`——否则界面会永远显示
    /// "进行中"，且新任务永远无法开始。
    #[test]
    fn guard_drop_without_finish_marks_failed() {
        let jobs = DownloadJobs::default();
        {
            let _guard = jobs.try_start(sample_request()).unwrap();
        }
        let snap = jobs.snapshot();
        assert_eq!(snap.state, "failed");
        assert_ne!(snap.state, "running");
        assert_eq!(snap.last_error.as_deref(), Some("任务意外结束"));
    }

    /// 显式 `finish()` 之后 drop，不应再被 `Drop` 兜底逻辑覆盖。
    #[test]
    fn explicit_finish_is_not_overridden_by_drop() {
        let jobs = DownloadJobs::default();
        let guard = jobs.try_start(sample_request()).unwrap();
        let snap = guard.finish("finished");
        assert_eq!(snap.state, "finished");
        assert_eq!(jobs.snapshot().state, "finished");
    }

    /// `cancel()`：没有任务时返回 `false`；有任务时返回 `true`，且必须
    /// 真的触发取消令牌，不能只是回报"有任务"却不做事。
    #[test]
    fn cancel_reflects_task_presence_and_triggers_token() {
        let jobs = DownloadJobs::default();
        assert!(!jobs.cancel(), "没有任务时应返回 false");

        let guard = jobs.try_start(sample_request()).unwrap();
        let token = guard.cancel_token();
        assert!(!token.is_cancelled());
        assert!(jobs.cancel(), "有任务时应返回 true");
        assert!(token.is_cancelled(), "cancel() 必须真的触发令牌");
    }

    // ---- 归档缓存 TTL ----

    #[test]
    fn cache_entry_uses_six_hour_ttl_on_success() {
        let stored = Utc::now();
        assert!(!is_cache_entry_stale(
            stored,
            stored + chrono::Duration::hours(5),
            true
        ));
        assert!(is_cache_entry_stale(
            stored,
            stored + chrono::Duration::hours(6),
            true
        ));
    }

    #[test]
    fn cache_entry_uses_thirty_second_ttl_on_failure() {
        let stored = Utc::now();
        assert!(!is_cache_entry_stale(
            stored,
            stored + chrono::Duration::seconds(29),
            false
        ));
        assert!(is_cache_entry_stale(
            stored,
            stored + chrono::Duration::seconds(30),
            false
        ));
    }
}
