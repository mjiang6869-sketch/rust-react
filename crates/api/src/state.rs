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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use domain::ServiceMode;
use engine::{EngineConfig, PaperEngine};
use rusqlite::Connection;
use tokio::sync::Mutex;

/// 后台任务推送给界面的进度消息。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressMessage {
    /// 下载进度。
    Download {
        symbol: String,
        kind: String,
        month: String,
        done: usize,
        total: usize,
    },
    /// 下载完成。
    DownloadDone { completed: usize, failed: usize },
    /// 回测进度。
    Backtest {
        symbol: String,
        model: String,
        done: usize,
        total: usize,
    },
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
}

impl AppState {
    pub fn new(engine: PaperEngine, db: Connection, data_root: PathBuf) -> Arc<Self> {
        let (progress_tx, _) = tokio::sync::broadcast::channel(256);
        Arc::new(Self {
            engine: Arc::new(Mutex::new(engine)),
            db: Arc::new(Mutex::new(db)),
            data_root,
            progress_tx,
            seen_idempotency_keys: Mutex::new(HashSet::new()),
            mode: Mutex::new(ServiceMode::Paper),
        })
    }

    /// 从配置构造。
    pub fn from_config(config: EngineConfig, db: Connection, data_root: PathBuf) -> Arc<Self> {
        Self::new(PaperEngine::new(config), db, data_root)
    }

    pub fn schema_version(&self) -> i32 {
        store::CURRENT_VERSION
    }

    pub async fn mode(&self) -> ServiceMode {
        *self.mode.lock().await
    }

    pub async fn set_mode(&self, mode: ServiceMode) {
        *self.mode.lock().await = mode;
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
        s.broadcast(ProgressMessage::DownloadDone {
            completed: 1,
            failed: 0,
        });
    }

    #[tokio::test]
    async fn progress_messages_are_serializable() {
        let msg = ProgressMessage::Download {
            symbol: "ETHUSDC".into(),
            kind: "klines".into(),
            month: "2026-08".into(),
            done: 3,
            total: 10,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"download\""), "{json}");
        assert!(json.contains("ETHUSDC"));
    }
}
