//! 把币安实时行情喂给模拟盘引擎。
//!
//! # 职责
//!
//! 本模块是行情推送（`exchange::stream`）与撮合引擎（`engine::PaperEngine`）
//! 之间唯一的胶水层：常驻订阅一份 [`exchange::EventFeed`]，把逐笔事件转发给
//! `PaperEngine::on_market_event`，并把链路的在线/离线状态同步给
//! `PaperEngine::set_feed_connected`。它不做任何交易判断——那些逻辑全部在
//! `engine` 与 `sim` 里，这里只负责"喂"和"报连接状态"。
//!
//! # 为什么用逐笔事件通道而不是 watch 视图
//!
//! `EventFeed::trades` / `EventFeed::klines` 是 `watch::Receiver<MarketView>`，
//! `watch` 通道只保留最新值——如果拿它驱动引擎，同一 tick 内的多笔成交会被
//! 合并成一条，撮合模型（M1）却需要**每一笔**成交去判定我们的挂单是否被
//! 吃到。所以真正驱动引擎的是 `EventFeed::events`（`broadcast::Receiver`），
//! 它按顺序投递每一笔新成交与每一根收盘 K 线；两个 `watch` 只用来读链路
//! 状态（`is_live()`）和保持上游连接不被回收。
//!
//! # 锁纪律
//!
//! 引擎锁只在同步调用引擎方法时持有（`on_market_event` / `drain_events` /
//! `set_feed_connected`），锁内不 `await` 任何东西——引擎处理里不涉及 I/O，
//! 而持锁期间阻塞的话，`/api/v1/ws` 与手动下单接口都会跟着卡住。
//!
//! # 断线缺口不补的取舍
//!
//! 断线期间发生的成交会永远丢失——重连后 `EventFeed` 不会补历史。真实实现
//! 应该用 REST 成交流补底（权重 20，本模块不做，留给未来任务）。这不是无害
//! 的简化：断线期间如果我们的挂单本该被吃到，模拟盘会误判为"没成交"。好在
//! `PaperEngine::set_feed_connected(false)` 会撤掉在途的开仓单，缺口只影响
//! "断线那一刻已经建立的持仓"要不要被判定为已触及止盈/止损——这类持仓在
//! 断线期间同样拿不到成交来源，风险敞口在断线告警里已经体现。
//!
//! # K 线周期固定为 1 分钟
//!
//! 与本地回测归档的粒度、以及 `domain::Candle::close_time()`（假定周期为
//! 1 分钟）保持一致；引擎的策略窗口也是按 1 分钟根数计的。如果将来支持多
//! 周期，`close_time()` 与这里的 `Interval::M1` 必须一起改。

use std::sync::Arc;
use std::time::Duration;

use exchange::{EventFeed, Interval, StreamKind};
use tokio::sync::Mutex;

use crate::state::AppState;
use engine::{EngineEvent, PaperEngine};

/// 预热拉取的历史 K 线根数。
///
/// 覆盖策略 `range_maker` 的默认 lookback（60 根），并与引擎 K 线窗口的
/// 下限（`EngineConfig::max_candles.max(120)`，见 `engine::paper`）对齐，
/// 尽量让服务一启动就有完整窗口可用，而不必等实时流攒够一小时。
const WARMUP_CANDLES: u32 = 120;

/// 预热失败时的最大重试次数。
const WARMUP_MAX_ATTEMPTS: u32 = 3;

/// 预热重试的最短等待（秒）；第 N 次重试至少等待 `10 * N` 秒。
const WARMUP_RETRY_BASE_SECS: u64 = 10;

/// 启动行情喂送：订阅默认交易对的逐笔事件与 1 分钟 K 线，并在后台跑
/// 预热与主循环。**必须在 tokio 运行时内调用**（内部会 `tokio::spawn`）。
///
/// `state.market_streams()` 为 `None`（行情 REST 地址被指到了非生产网）时，
/// 引擎不会收到任何行情——模拟盘将永远不会成交。这种情况只记一条警告并
/// 直接返回，不阻止服务启动。
pub fn spawn(state: Arc<AppState>) {
    crate::overview::spawn_sampling(state.clone());
    let Some(streams) = state.market_streams() else {
        tracing::warn!(
            "行情推送未启用（行情地址指向了非生产网）：模拟盘引擎收不到实时行情，不会产生任何成交"
        );
        return;
    };

    let symbol = state.symbol();
    let feed = streams.subscribe_events(&symbol, Interval::M1);

    tokio::spawn(warm_up(state.clone()));
    tokio::spawn(run(state.engine.clone(), feed));
}

/// 用 REST 历史 K 线为策略预热窗口。只做"补窗口"这一件事，不影响新鲜度
/// 判定——预热逻辑本身在 `PaperEngine::seed_candles` 里已有详细说明。
async fn warm_up(state: Arc<AppState>) {
    let Some(client) = state.market() else {
        tracing::info!("行情客户端未启用，跳过 K 线预热，等待实时流自行攒够窗口");
        return;
    };
    let symbol = state.symbol();

    for attempt in 1..=WARMUP_MAX_ATTEMPTS {
        match client.klines(&symbol, Interval::M1, WARMUP_CANDLES).await {
            Ok(candles) => {
                let mut engine = state.engine.lock().await;
                let added = engine.seed_candles(candles);
                let total = engine.candle_count();
                drop(engine);
                tracing::info!(symbol, added, total, "K 线预热完成");
                return;
            }
            Err(e) => {
                tracing::warn!(symbol, attempt, error = %e, "K 线预热失败");
            }
        }

        if attempt == WARMUP_MAX_ATTEMPTS {
            break;
        }
        let cooldown_wait = state.market_cooldown().remaining_ms().unwrap_or(0);
        let backoff_wait = WARMUP_RETRY_BASE_SECS * 1_000 * u64::from(attempt);
        let wait_ms = cooldown_wait.max(backoff_wait);
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    }

    tracing::warn!(
        symbol,
        "预热失败，策略将在实时 K 线攒够窗口后开始工作（约 60 分钟）"
    );
}

/// 核心循环：把逐笔事件转发给引擎，并同步链路在线/离线状态。
///
/// `pub` 是为了让集成测试能注入手工构造的 `EventFeed`（不连网）。
pub async fn run(engine: Arc<Mutex<PaperEngine>>, mut feed: EventFeed) {
    let mut last_alarm: Option<String> = None;
    // 初始值与引擎构造时的默认状态一致（`PaperEngine::new` 里 `feed_connected`
    // 默认为 `false`），所以只有链路已经在线时才需要在进入循环前补一次同步。
    let mut last_live = false;
    sync_link_state(
        &engine,
        &mut last_live,
        feed.is_live(),
        &feed,
        &mut last_alarm,
    )
    .await;

    let mut trades_closed = false;
    let mut klines_closed = false;

    loop {
        tokio::select! {
            event = feed.events.recv() => {
                match event {
                    Ok(ev) => {
                        let events = {
                            let mut e = engine.lock().await;
                            e.on_market_event(ev);
                            e.drain_events()
                        };
                        log_events(events, &mut last_alarm);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            skipped = n,
                            "行情事件广播跟不上，已丢失 {n} 条行情事件；成交判定可能不完整"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("行情事件广播已关闭，停止喂送");
                        break;
                    }
                }
            }
            // 只要发送端还没关闭就继续监听那一路；关闭后不再把这个分支纳入
            // `select!` 的轮询（守卫为 false），避免对一个永远返回 `Err`
            // 的 future 忙轮询。
            changed = feed.trades.changed(), if !trades_closed => {
                if changed.is_err() {
                    trades_closed = true;
                    mark_offline(&engine, &mut last_live, &mut last_alarm).await;
                    continue;
                }
                sync_link_state(&engine, &mut last_live, feed.is_live(), &feed, &mut last_alarm).await;
            }
            changed = feed.klines.changed(), if !klines_closed => {
                if changed.is_err() {
                    klines_closed = true;
                    mark_offline(&engine, &mut last_live, &mut last_alarm).await;
                    continue;
                }
                sync_link_state(&engine, &mut last_live, feed.is_live(), &feed, &mut last_alarm).await;
            }
        }
    }

    if last_live {
        mark_offline(&engine, &mut last_live, &mut last_alarm).await;
    }
}

/// 无条件把引擎标记为未连接（用于 watch 发送端关闭、循环退出等场景）。
async fn mark_offline(
    engine: &Arc<Mutex<PaperEngine>>,
    last_live: &mut bool,
    last_alarm: &mut Option<String>,
) {
    if !*last_live {
        return;
    }
    *last_live = false;
    let mut e = engine.lock().await;
    e.set_feed_connected(false);
    let events = e.drain_events();
    drop(e);
    log_events(events, last_alarm);
    tracing::warn!("行情链路已断开：模拟盘暂停开仓");
}

/// 把当前链路状态同步给引擎（仅在状态变化时才加锁），并记一条日志。
async fn sync_link_state(
    engine: &Arc<Mutex<PaperEngine>>,
    last_live: &mut bool,
    live: bool,
    feed: &EventFeed,
    last_alarm: &mut Option<String>,
) {
    if live == *last_live {
        return;
    }
    *last_live = live;

    {
        let mut e = engine.lock().await;
        e.set_feed_connected(live);
        let events = e.drain_events();
        drop(e);
        log_events(events, last_alarm);
    }

    if live {
        tracing::info!("行情链路已恢复：成交流与 K 线流均在线");
    } else {
        let reason = [
            feed.trades
                .borrow()
                .trades_link
                .describe(StreamKind::Trades),
            feed.klines
                .borrow()
                .kline_link
                .describe(StreamKind::Kline(Interval::M1)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("；");
        tracing::warn!(reason, "行情链路已断开：模拟盘暂停开仓");
    }
}

/// 记录引擎产生的事件。`StateChanged` 不记日志（界面轮询快照即可）。
fn log_events(events: Vec<EngineEvent>, last_alarm: &mut Option<String>) {
    for ev in events {
        match ev {
            EngineEvent::Filled {
                order,
                quantity,
                price,
                fee,
            } => {
                tracing::info!(%order, %quantity, %price, %fee, "成交");
            }
            EngineEvent::TradeClosed {
                entry_price,
                exit_price,
                quantity,
                pnl,
                exit_reason,
            } => {
                tracing::info!(
                    ?exit_reason,
                    %entry_price,
                    %exit_price,
                    %quantity,
                    %pnl,
                    "交易结束"
                );
            }
            EngineEvent::Alarm(text) => {
                // 止损触发未成交时，每笔新成交都会重复产生同样的告警文本，
                // 与上一条相同就不重复打——否则日志会被同一条告警刷满。
                if last_alarm.as_deref() != Some(text.as_str()) {
                    tracing::warn!(%text, "引擎告警");
                    *last_alarm = Some(text);
                }
            }
            EngineEvent::Rejected { reason } => {
                // 策略让位（例如已有持仓、风控拒绝）每根 K 线都可能产生，
                // 属于正常噪音，降级为 debug。
                tracing::debug!(%reason, "策略意图被拒绝");
            }
            EngineEvent::StateChanged => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use domain::{
        AggTrade, ContractKind, FeeSchedule, FeeSource, Instrument, ManualPlan, Precision, Price,
        Qty, RiskLimits, Side, TpPlan, TpRung,
    };
    use engine::{EngineConfig, PaperEngine, SubmitOutcome};
    use exchange::{LinkState, MarketView};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use tokio::sync::{broadcast, watch};

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
                observed_at: Utc::now(),
            },
        }
    }

    fn config() -> EngineConfig {
        EngineConfig {
            instrument: instrument(),
            limits: RiskLimits {
                max_stop_pct: dec!(0.05),
                min_reward_risk: Decimal::ONE,
                max_feed_staleness_secs: 15,
            },
            initial_equity: dec!(10000),
            assumed_latency_ms: 100,
            max_candles: 120,
            max_staleness_secs: 15,
        }
    }

    fn t0() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_785_542_400_000).unwrap()
    }

    /// 参考 `engine::paper` 测试里的手动计划：ETHUSDC、入场 3200、
    /// 止损 3192、两档止盈、数量 0.1。
    fn manual_plan() -> ManualPlan {
        ManualPlan {
            symbol: "ETHUSDC".into(),
            side: Side::Buy,
            entry: dec!(3200),
            quantity: Some(Qty::new(dec!(0.1))),
            size_pct: None,
            leverage: Decimal::from(3),
            stop: dec!(3192),
            take_profit: TpPlan::Ladder {
                rungs: vec![
                    TpRung {
                        pct: dec!(0.0025),
                        fraction: dec!(0.5),
                    },
                    TpRung {
                        pct: dec!(0.005),
                        fraction: dec!(0.5),
                    },
                ],
            },
            break_even: None,
            trailing: None,
            cancel_unfilled_after: Some(t0() + ChronoDuration::minutes(2)),
            client_ref: "manual".into(),
        }
    }

    fn trade_event(offset_ms: i64, px: Decimal, buyer_maker: bool) -> domain::MarketEvent {
        domain::MarketEvent::AggTrade(AggTrade {
            trade_id: offset_ms.unsigned_abs() + 1,
            price: Price::new(px),
            quantity: Qty::new(dec!(1)),
            is_buyer_maker: buyer_maker,
            at: t0() + ChronoDuration::milliseconds(offset_ms),
        })
    }

    /// 手工构造一份 `EventFeed`：不连网，供测试直接驱动 `run`。
    fn fake_feed() -> (
        EventFeed,
        broadcast::Sender<domain::MarketEvent>,
        watch::Sender<MarketView>,
        watch::Sender<MarketView>,
    ) {
        let (events_tx, events_rx) = broadcast::channel(16);
        let (trades_tx, trades_rx) = watch::channel(MarketView::default());
        let (klines_tx, klines_rx) = watch::channel(MarketView::default());
        let feed = EventFeed {
            events: events_rx,
            trades: trades_rx,
            klines: klines_rx,
        };
        (feed, events_tx, trades_tx, klines_tx)
    }

    async fn wait_until<F: Fn() -> bool>(pred: F) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !pred() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("等待条件超时");
    }

    fn live_view() -> MarketView {
        MarketView {
            trades_link: LinkState::Live,
            kline_link: LinkState::Live,
            ..MarketView::default()
        }
    }

    /// 两条链路都上线后引擎应标记为已连接；任一条掉线后应变回未连接。
    #[tokio::test]
    async fn feed_live_state_drives_engine_connection() {
        let engine = Arc::new(Mutex::new(PaperEngine::new(config())));
        let (feed, _events_tx, trades_tx, klines_tx) = fake_feed();

        let handle = tokio::spawn(run(engine.clone(), feed));

        trades_tx.send_modify(|v| v.trades_link = LinkState::Live);
        klines_tx.send_modify(|v| v.kline_link = LinkState::Live);

        wait_until(|| {
            let e = engine.try_lock().expect("测试单线程访问不应竞争");
            e.snapshot().feed_connected
        })
        .await;

        trades_tx.send_modify(|v| {
            v.trades_link = LinkState::Retrying {
                attempt: 1,
                retry_in_ms: 500,
                reason: "测试断线".into(),
            }
        });

        wait_until(|| {
            let e = engine.try_lock().expect("测试单线程访问不应竞争");
            !e.snapshot().feed_connected
        })
        .await;

        handle.abort();
    }

    /// 广播的成交事件必须能驱动引擎成交：上线后挂一张买单，收到打在
    /// 我方买价上的卖方主动成交后应建立持仓。
    #[tokio::test]
    async fn trade_events_reach_engine_and_fill_order() {
        let engine = Arc::new(Mutex::new(PaperEngine::new(config())));
        let (feed, events_tx, trades_tx, klines_tx) = fake_feed();
        trades_tx.send(live_view()).expect("发送视图");
        klines_tx.send(live_view()).expect("发送视图");

        let handle = tokio::spawn(run(engine.clone(), feed));

        wait_until(|| {
            let e = engine.try_lock().expect("测试单线程访问不应竞争");
            e.snapshot().feed_connected
        })
        .await;

        {
            let mut e = engine.lock().await;
            let out = e.submit_manual(&manual_plan(), t0());
            assert!(matches!(out, SubmitOutcome::Accepted(_)), "{out:?}");
        }

        events_tx
            .send(trade_event(1_000, dec!(3200), true))
            .expect("广播成交");

        wait_until(|| {
            let e = engine.try_lock().expect("测试单线程访问不应竞争");
            e.snapshot().position.is_some()
        })
        .await;

        handle.abort();
    }

    /// 慢消费者被 `Lagged` 跳过部分事件后，循环必须继续处理后续事件，
    /// 而不是卡住或退出。
    #[tokio::test]
    async fn lagged_receiver_does_not_stop_the_loop() {
        let engine = Arc::new(Mutex::new(PaperEngine::new(config())));
        let (events_tx, events_rx) = broadcast::channel(2);
        let (trades_tx, trades_rx) = watch::channel(live_view());
        let (klines_tx, klines_rx) = watch::channel(live_view());
        let feed = EventFeed {
            events: events_rx,
            trades: trades_rx,
            klines: klines_rx,
        };
        let _ = (&trades_tx, &klines_tx);

        // 容量只有 2：塞 3 条超过容量的事件，触发 Lagged。
        for i in 0..3 {
            let _ = events_tx.send(trade_event(i, dec!(3100), true));
        }

        let handle = tokio::spawn(run(engine.clone(), feed));

        let last = trade_event(9_999, dec!(3105), true);
        events_tx.send(last.clone()).expect("广播最后一笔");

        wait_until(|| {
            let e = engine.try_lock().expect("测试单线程访问不应竞争");
            e.snapshot().last_event_at == Some(last.at())
        })
        .await;

        handle.abort();
    }

    /// 广播发送端关闭后循环应结束，且引擎被标记为未连接。
    #[tokio::test]
    async fn closed_event_channel_ends_loop_and_marks_disconnected() {
        let engine = Arc::new(Mutex::new(PaperEngine::new(config())));
        let (feed, events_tx, trades_tx, klines_tx) = fake_feed();
        trades_tx.send(live_view()).expect("发送视图");
        klines_tx.send(live_view()).expect("发送视图");

        let handle = tokio::spawn(run(engine.clone(), feed));

        wait_until(|| {
            let e = engine.try_lock().expect("测试单线程访问不应竞争");
            e.snapshot().feed_connected
        })
        .await;

        drop(events_tx);

        tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("循环应在超时内结束")
            .expect("任务不应 panic");

        let e = engine.lock().await;
        assert!(!e.snapshot().feed_connected, "循环结束后应标记为未连接");
    }
}
