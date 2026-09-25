pub mod ai;
pub mod analysis;
pub mod backtest;
pub mod binance;
pub mod execution;
mod feed;
pub mod live;
mod model;
pub mod order_state;
mod paper;
mod signal;
mod storage;
pub mod user_stream;

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post, put},
};
use chrono::Utc;
use futures_util::StreamExt;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::ai::{AnalysisRequest, AnalysisResponse};
use crate::analysis::TrendAnalysis;
use crate::backtest::{BacktestConfig, BacktestReport, FillModel, OrderBookSnapshot};
use crate::feed::{BinanceFeed, parse_ws_candle};
use crate::live::{LiveOrderRules, LiveReadiness, LiveRuntime, LiveStatus};
use crate::model::Candle;
use crate::paper::{Config, ExecutionMode, PaperEngine, Snapshot, Stored};

#[derive(Clone)]
struct AppState {
    engine: Arc<Mutex<PaperEngine>>,
    path: Arc<PathBuf>,
    live: Arc<Mutex<Option<LiveRuntime>>>,
}

#[derive(serde::Deserialize)]
struct BacktestRequest {
    candles: Vec<Candle>,
    #[serde(default)]
    order_book: Vec<OrderBookSnapshot>,
}

#[derive(serde::Deserialize)]
struct ModeRequest {
    mode: ExecutionMode,
}

type ApiResult<T> = Result<T, (StatusCode, String)>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rust_crypto=info".into()),
        )
        .init();
    let symbol = std::env::var("RUST_CRYPTO_SYMBOL")
        .unwrap_or_else(|_| "ETHUSDC".to_string())
        .trim()
        .to_uppercase();
    if symbol.is_empty()
        || !symbol
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        bail!("交易对只能包含大写字母和数字");
    }
    let path = PathBuf::from(
        std::env::var("RUST_CRYPTO_STATE_PATH").unwrap_or_else(|_| format!("data/{symbol}.json")),
    );
    let _state_lock = storage::lock(&path)?;
    let saved = storage::load(&path)?;
    let feed = BinanceFeed::new(symbol.clone())?;
    let contract = feed
        .contract_info()
        .await
        .context("无法核验交易所精度，拒绝启动模拟盘")?;
    let fresh_state = saved.is_none();
    let mut stored = saved.unwrap_or_else(|| Stored::initial(symbol.clone(), Utc::now()));
    if stored.config.symbol != symbol {
        bail!("持久化交易对与行情交易对不一致");
    }
    stored.config.quote_asset = contract.quote_asset;
    stored.config.margin_asset = contract.margin_asset;
    stored.config.contract_type = contract.contract_type;
    if fresh_state
        && stored.config.contract_type == "PERPETUAL"
        && stored.config.margin_asset == "USDT"
    {
        stored.config.maker_fee_pct = rust_decimal::Decimal::new(2, 2);
    }
    stored.config.tick_size = contract.tick_size;
    stored.config.step_size = contract.step_size;
    stored.config.min_qty = contract.min_qty;
    stored.config.min_notional = contract.min_notional;
    stored.config.validate().map_err(anyhow::Error::msg)?;
    storage::save(&path, &stored)?;
    let state = AppState {
        engine: Arc::new(Mutex::new(PaperEngine::new(stored))),
        path: Arc::new(path),
        live: Arc::new(Mutex::new(None)),
    };
    let router = Router::new()
        .route("/api/health", get(health))
        .route("/api/state", get(get_state))
        .route("/api/history", get(get_history))
        .route("/api/analysis", get(get_analysis))
        .route("/api/live/readiness", get(get_live_readiness))
        .route("/api/live/status", get(get_live_status))
        .route("/api/live/connect", post(connect_live))
        .route("/api/live/arm", post(arm_live))
        .route("/api/live/disarm", post(disarm_live))
        .route("/api/live/close", post(close_live))
        .route("/api/ai/analyze", post(ai_analyze))
        .route("/api/backtest", post(run_backtest))
        .route("/api/config", put(update_config))
        .route("/api/mode", put(update_mode))
        .route("/api/kill", post(kill))
        .with_state(state.clone());
    let bind = std::env::var("RUST_CRYPTO_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!("模拟盘 API 监听 {bind}，交易对 {symbol}");
    tokio::select! {
        result = axum::serve(listener, router) => result.context("API 服务退出")?,
        result = run_feed(feed, state) => result.context("行情任务退出")?,
    }
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn get_state(State(state): State<AppState>) -> Json<Snapshot> {
    Json(state.engine.lock().await.snapshot(Utc::now()))
}

async fn get_history(State(state): State<AppState>) -> Json<Vec<Candle>> {
    let engine = state.engine.lock().await;
    Json(engine.history.values().cloned().collect())
}

async fn get_analysis(State(state): State<AppState>) -> Json<TrendAnalysis> {
    let engine = state.engine.lock().await;
    let candles: Vec<Candle> = engine.history.values().cloned().collect();
    Json(analysis::analyze(&candles))
}

async fn get_live_readiness(State(state): State<AppState>) -> Json<LiveReadiness> {
    let engine = state.engine.lock().await;
    Json(live::readiness(engine.stored.mode))
}

async fn get_live_status(State(state): State<AppState>) -> Json<LiveStatus> {
    let live = state.live.lock().await;
    Json(live.as_ref().map_or_else(
        || LiveStatus {
            runtime_created: false,
            user_stream_connected: false,
            account_reconciled: false,
            armed: false,
            unresolved_order_ids: Vec::new(),
            available_collateral: rust_decimal::Decimal::ZERO,
            message: "LIVE runtime 尚未创建".to_string(),
        },
        LiveRuntime::status,
    ))
}

async fn connect_live(State(state): State<AppState>) -> ApiResult<Json<LiveStatus>> {
    let (symbol, mode, margin_asset, rules, saved_live_orders) = {
        let engine = state.engine.lock().await;
        (
            engine.stored.config.symbol.clone(),
            engine.stored.mode,
            engine.stored.config.margin_asset.clone(),
            LiveOrderRules {
                tick_size: engine.stored.config.tick_size,
                step_size: engine.stored.config.step_size,
                min_qty: engine.stored.config.min_qty,
                min_notional: engine.stored.config.min_notional,
                take_profit_pct: engine.stored.config.take_profit_pct,
            },
            engine.stored.live_orders.clone(),
        )
    };
    if mode != ExecutionMode::Live {
        return Err((
            StatusCode::CONFLICT,
            "当前仍是 PAPER 模式，请先显式切换到 LIVE".to_string(),
        ));
    }
    if let Some(runtime) = state.live.lock().await.as_ref() {
        return Ok(Json(runtime.status()));
    }
    let mut runtime = LiveRuntime::from_env(symbol.clone(), mode, rules).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("创建 LIVE runtime 失败：{error:#}"),
        )
    })?;
    if let Err(error) = runtime.connect().await {
        let _ = runtime.close().await;
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("连接 Binance 用户数据流失败：{error:#}"),
        ));
    }
    if let Err(error) = runtime.reconcile_account(&symbol, &margin_asset).await {
        let _ = runtime.close().await;
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Binance 账户对账失败：{error:#}"),
        ));
    }
    if let Err(error) = runtime.restore_orders(&saved_live_orders, Utc::now()).await {
        let _ = runtime.close().await;
        return Err((
            StatusCode::CONFLICT,
            format!("LIVE 重启订单恢复失败：{error:#}"),
        ));
    }
    let status = runtime.status();
    let mut live = state.live.lock().await;
    if let Some(existing) = live.as_ref() {
        let existing_status = existing.status();
        drop(live);
        let _ = runtime.close().await;
        return Ok(Json(existing_status));
    }
    *live = Some(runtime);
    tokio::spawn(run_live_supervisor(state.clone()));
    Ok(Json(status))
}

async fn run_live_supervisor(state: AppState) {
    let mut last_keepalive = tokio::time::Instant::now();
    loop {
        let mut guard = state.live.lock().await;
        let Some(runtime) = guard.as_mut() else {
            return;
        };
        let event =
            tokio::time::timeout(Duration::from_secs(1), runtime.next_reconcile_action()).await;
        if let Err(error) = runtime.cancel_expired_entries(Utc::now()).await {
            runtime.disarm();
            warn!("LIVE 过期开仓单撤单或对账失败，已自动 DISARM: {error:#}");
        }
        match event {
            Ok(Ok(Some(crate::order_state::ReconcileAction::MarkFilled))) => {
                if let Some(client_order_id) =
                    runtime.last_event_client_order_id().map(str::to_owned)
                {
                    if let Err(error) = runtime
                        .submit_protection_for(&client_order_id, Utc::now())
                        .await
                    {
                        runtime.disarm();
                        warn!("LIVE 成交后保护单提交失败，已自动 DISARM: {error:#}");
                    } else if let Err(error) =
                        runtime.cancel_protection_sibling(&client_order_id).await
                    {
                        runtime.disarm();
                        warn!("LIVE 保护单互斥撤单失败，已自动 DISARM: {error:#}");
                    }
                }
            }
            Ok(Ok(Some(crate::order_state::ReconcileAction::Alert))) => {
                runtime.disarm();
                warn!("LIVE 订单对账异常，已自动 DISARM");
            }
            Ok(Ok(Some(crate::order_state::ReconcileAction::QueryOrder))) => {
                let ids = runtime.unresolved_order_ids();
                for client_order_id in ids {
                    match runtime.reconcile_order(&client_order_id, Utc::now()).await {
                        Ok(crate::order_state::ReconcileAction::MarkFilled) => {
                            if let Err(error) = runtime
                                .submit_protection_for(&client_order_id, Utc::now())
                                .await
                            {
                                runtime.disarm();
                                warn!(
                                    "LIVE 查询确认成交后保护单提交失败，已自动 DISARM: {error:#}"
                                );
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            runtime.disarm();
                            warn!("LIVE 订单查询失败，已自动 DISARM: {error:#}");
                            break;
                        }
                    }
                }
            }
            Ok(Ok(_)) | Err(_) => {}
            Ok(Err(error)) => {
                runtime.disarm();
                warn!("LIVE 用户数据流异常，已自动 DISARM: {error:#}");
            }
        }
        if last_keepalive.elapsed() >= Duration::from_secs(20 * 60) {
            last_keepalive = tokio::time::Instant::now();
            if let Err(error) = runtime.keepalive().await {
                runtime.disarm();
                warn!("LIVE 用户数据流 keepalive 失败，已自动 DISARM: {error:#}");
            }
        }
        let submitted = runtime.submitted_orders();
        drop(guard);
        if let Err(error) = persist_live_orders(&state, submitted).await {
            if let Some(runtime) = state.live.lock().await.as_mut() {
                runtime.disarm();
            }
            warn!("LIVE 订单状态持久化失败，已自动 DISARM: {error:#}");
        }
    }
}

async fn persist_live_orders(
    state: &AppState,
    orders: Vec<crate::execution::MakerOrder>,
) -> Result<()> {
    let mut engine = state.engine.lock().await;
    engine.stored.live_orders = orders;
    storage::save(&state.path, &engine.stored)
}

async fn arm_live(State(state): State<AppState>) -> ApiResult<Json<LiveStatus>> {
    let mut live = state.live.lock().await;
    let runtime = live.as_mut().ok_or((
        StatusCode::CONFLICT,
        "LIVE runtime 尚未创建，不能 arm".to_string(),
    ))?;
    runtime
        .arm()
        .map_err(|error| (StatusCode::CONFLICT, format!("LIVE arm 被拒绝：{error:#}")))?;
    Ok(Json(runtime.status()))
}

async fn disarm_live(State(state): State<AppState>) -> ApiResult<Json<LiveStatus>> {
    let mut live = state.live.lock().await;
    let runtime = live
        .as_mut()
        .ok_or((StatusCode::CONFLICT, "LIVE runtime 尚未创建".to_string()))?;
    runtime.disarm();
    runtime
        .cancel_open_entries(Utc::now())
        .await
        .map_err(|error| {
            (
                StatusCode::BAD_GATEWAY,
                format!("DISARM 后撤销 LIVE 开仓单失败：{error:#}"),
            )
        })?;
    Ok(Json(runtime.status()))
}

async fn close_live(State(state): State<AppState>) -> ApiResult<Json<LiveStatus>> {
    let runtime = state.live.lock().await.take();
    let Some(mut runtime) = runtime else {
        return Ok(Json(LiveStatus {
            runtime_created: false,
            user_stream_connected: false,
            account_reconciled: false,
            armed: false,
            unresolved_order_ids: Vec::new(),
            available_collateral: rust_decimal::Decimal::ZERO,
            message: "LIVE runtime 尚未创建".to_string(),
        }));
    };
    runtime.disarm();
    runtime.close().await.map_err(|error| {
        (
            StatusCode::BAD_GATEWAY,
            format!("关闭 LIVE runtime 失败：{error:#}"),
        )
    })?;
    Ok(Json(LiveStatus {
        runtime_created: false,
        user_stream_connected: false,
        account_reconciled: false,
        armed: false,
        unresolved_order_ids: Vec::new(),
        available_collateral: rust_decimal::Decimal::ZERO,
        message: "LIVE runtime 已关闭".to_string(),
    }))
}

async fn ai_analyze(
    State(state): State<AppState>,
    Json(request): Json<AnalysisRequest>,
) -> ApiResult<Json<AnalysisResponse>> {
    let engine = state.engine.lock().await;
    let candles: Vec<Candle> = engine.history.values().cloned().collect();
    let context = analysis::analyze(&candles);
    drop(engine);
    ai::analyze(request.question, &context)
        .await
        .map(Json)
        .map_err(|error| (StatusCode::BAD_GATEWAY, format!("AI 分析失败：{error}")))
}

async fn run_backtest(
    State(state): State<AppState>,
    Json(request): Json<BacktestRequest>,
) -> ApiResult<Json<BacktestReport>> {
    if request.candles.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "回测至少需要一根已收盘 K 线".to_string(),
        ));
    }
    let engine = state.engine.lock().await;
    let config = &engine.stored.config;
    let order_book: std::collections::BTreeMap<chrono::DateTime<Utc>, OrderBookSnapshot> = request
        .order_book
        .into_iter()
        .map(|snapshot| (snapshot.open_time, snapshot))
        .collect();
    let backtest_config = BacktestConfig {
        initial_equity: engine.available_collateral_for_backtest(),
        margin_pct: config.margin_pct,
        leverage: config.leverage,
        stop_pct: config.stop_pct,
        take_profit_pct: config.take_profit_pct,
        maker_fee_pct: config.maker_fee_pct,
        tick_size: config.tick_size,
        step_size: config.step_size,
        min_qty: config.min_qty,
        min_notional: config.min_notional,
        fill_model: if order_book.is_empty() {
            FillModel::CandleRangeTouch
        } else {
            FillModel::TopOfBook
        },
        order_book,
    };
    Ok(Json(backtest::run(&request.candles, &backtest_config)))
}

async fn update_config(
    State(state): State<AppState>,
    Json(config): Json<Config>,
) -> ApiResult<Json<Snapshot>> {
    let mut engine = state.engine.lock().await;
    let previous = engine.stored.clone();
    engine
        .update_config(config, Utc::now())
        .map_err(|reason| (StatusCode::BAD_REQUEST, reason.to_string()))?;
    if let Err(error) = storage::save(&state.path, &engine.stored) {
        engine.stored = previous;
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("保存配置失败: {error}"),
        ));
    }
    Ok(Json(engine.snapshot(Utc::now())))
}

async fn update_mode(
    State(state): State<AppState>,
    Json(request): Json<ModeRequest>,
) -> ApiResult<Json<Snapshot>> {
    let current_mode = state.engine.lock().await.stored.mode;
    if current_mode == request.mode {
        return Ok(Json(state.engine.lock().await.snapshot(Utc::now())));
    }
    if request.mode == ExecutionMode::Live
        && !live::readiness(ExecutionMode::Live).can_create_runtime
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "LIVE 未就绪：请检查允许的 Binance endpoint 和凭据配置".to_string(),
        ));
    }
    let engine = state.engine.lock().await;
    if engine.stored.position.is_some()
        || engine.stored.orders.iter().any(|order| {
            matches!(
                order.status,
                crate::paper::OrderStatus::Open | crate::paper::OrderStatus::Triggered
            )
        })
    {
        return Err((
            StatusCode::CONFLICT,
            "存在持仓或活动订单，不能切换执行模式".to_string(),
        ));
    }
    drop(engine);
    if request.mode == ExecutionMode::Paper {
        let unresolved = state
            .live
            .lock()
            .await
            .as_ref()
            .map(|runtime| runtime.unresolved_order_ids())
            .unwrap_or_default();
        if !unresolved.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                "仍有未对账 LIVE 订单，不能切回 PAPER".to_string(),
            ));
        }
        let runtime = state.live.lock().await.take();
        if let Some(mut runtime) = runtime {
            runtime.disarm();
            runtime.close().await.map_err(|error| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("关闭 LIVE runtime 失败：{error:#}"),
                )
            })?;
        }
    }
    let mut engine = state.engine.lock().await;
    let previous = engine.stored.mode;
    engine.stored.mode = request.mode;
    if let Err(error) = storage::save(&state.path, &engine.stored) {
        engine.stored.mode = previous;
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("保存执行模式失败：{error}"),
        ));
    }
    Ok(Json(engine.snapshot(Utc::now())))
}

async fn kill(State(state): State<AppState>) -> ApiResult<Json<Snapshot>> {
    let mut engine = state.engine.lock().await;
    let previous = engine.stored.clone();
    engine.kill(Utc::now());
    if let Err(error) = storage::save(&state.path, &engine.stored) {
        engine.stored = previous;
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("停用状态保存失败: {error}"),
        ));
    }
    let snapshot = engine.snapshot(Utc::now());
    drop(engine);
    if let Some(runtime) = state.live.lock().await.as_mut() {
        runtime.disarm();
        if let Err(error) = runtime.cancel_open_entries(Utc::now()).await {
            warn!("LIVE kill 后撤销开仓单失败，保持 DISARM: {error:#}");
        }
    }
    Ok(Json(snapshot))
}

async fn submit_live_entry_if_ready(state: &AppState, now: chrono::DateTime<Utc>) -> Result<()> {
    let collateral = {
        let live = state.live.lock().await;
        let Some(runtime) = live.as_ref() else {
            return Ok(());
        };
        if !runtime.status().armed {
            return Ok(());
        }
        runtime.available_collateral()
    };
    if collateral <= rust_decimal::Decimal::ZERO {
        return Ok(());
    }
    let intent = {
        let mut engine = state.engine.lock().await;
        let intent = engine.live_entry_intent(now, collateral);
        if intent.is_some() {
            storage::save(&state.path, &engine.stored)?;
        }
        intent
    };
    let Some(intent) = intent else {
        return Ok(());
    };
    {
        let mut engine = state.engine.lock().await;
        if !engine
            .stored
            .live_orders
            .iter()
            .any(|order| order.client_order_id == intent.client_order_id)
        {
            engine.stored.live_orders.push(intent.clone());
            storage::save(&state.path, &engine.stored)?;
        }
    }
    let mut live = state.live.lock().await;
    let Some(runtime) = live.as_mut() else {
        return Ok(());
    };
    if runtime.tracks_order(&intent.client_order_id) {
        return Ok(());
    }
    if let Err(error) = runtime.submit(intent, now).await {
        runtime.disarm();
        warn!("LIVE Maker 开仓提交失败，已自动 DISARM: {error:#}");
    }
    let submitted = runtime.submitted_orders();
    drop(live);
    persist_live_orders(state, submitted).await?;
    Ok(())
}

async fn run_feed(feed: BinanceFeed, state: AppState) -> Result<()> {
    let mut retry = 1;
    loop {
        match feed.history().await {
            Ok(history) => state.engine.lock().await.seed_history(history),
            Err(error) => warn!("历史 K 线回补失败: {error:#}"),
        }
        match feed.connect().await {
            Ok(mut socket) => {
                info!("公开行情 WebSocket 已连接");
                retry = 1;
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                let mut last_ws_at = tokio::time::Instant::now();
                let mut last_rest_at = tokio::time::Instant::now() - Duration::from_secs(5);
                {
                    let mut engine = state.engine.lock().await;
                    engine.set_connected(true, Utc::now());
                }
                loop {
                    tokio::select! {
                    _ = interval.tick() => {
                        if last_ws_at.elapsed() >= Duration::from_secs(5)
                            && last_rest_at.elapsed() >= Duration::from_secs(5)
                        {
                            last_rest_at = tokio::time::Instant::now();
                            match feed.latest().await {
                                Ok(candle) => {
                                    last_ws_at = tokio::time::Instant::now();
                                    let mut engine = state.engine.lock().await;
                                    if engine.on_candle(candle, Utc::now()) {
                                        storage::save(&state.path, &engine.stored)?;
                                    }
                                    drop(engine);
                                    submit_live_entry_if_ready(&state, Utc::now()).await?;
                                }
                                Err(error) => warn!("REST K 线兜底失败: {error:#}"),
                            }
                        }
                        if last_ws_at.elapsed() > Duration::from_secs(30) {
                            warn!("行情 30 秒未响应，准备重连");
                            break;
                        }
                        let mut engine = state.engine.lock().await;
                        if engine.drive(Utc::now()) {
                            storage::save(&state.path, &engine.stored)?;
                        }
                        drop(engine);
                        submit_live_entry_if_ready(&state, Utc::now()).await?;
                    }
                    message = socket.next() => match message {
                        Some(Ok(message)) => {
                            last_ws_at = tokio::time::Instant::now();
                            if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                                let symbol = state.engine.lock().await.stored.config.symbol.clone();
                                match parse_ws_candle(&text, &symbol) {
                                    Ok(Some(candle)) => {
                                        let mut engine = state.engine.lock().await;
                                        if engine.on_candle(candle, Utc::now()) {
                                            storage::save(&state.path, &engine.stored)?;
                                        }
                                        drop(engine);
                                        submit_live_entry_if_ready(&state, Utc::now()).await?;
                                    }
                                    Ok(None) => {},
                                    Err(error) => warn!("忽略无效 K 线消息: {error:#}"),
                                }
                            }
                        }
                        Some(Err(error)) => { warn!("行情连接错误: {error}"); break; }
                        None => { warn!("行情连接已关闭"); break; }
                    }
                    }
                }
            }
            Err(error) => warn!("行情连接失败: {error:#}"),
        }
        {
            let mut engine = state.engine.lock().await;
            if engine.set_connected(false, Utc::now()) {
                storage::save(&state.path, &engine.stored)?;
            }
        }
        let delay = retry.min(30);
        retry = (retry * 2).min(30);
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}
