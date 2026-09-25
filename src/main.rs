pub mod ai;
pub mod analysis;
pub mod backtest;
pub mod binance;
pub mod execution;
mod feed;
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
use crate::backtest::{BacktestConfig, BacktestReport, FillModel};
use crate::feed::{BinanceFeed, parse_ws_candle};
use crate::model::Candle;
use crate::paper::{Config, PaperEngine, Snapshot, Stored};

#[derive(Clone)]
struct AppState {
    engine: Arc<Mutex<PaperEngine>>,
    path: Arc<PathBuf>,
}

#[derive(serde::Deserialize)]
struct BacktestRequest {
    candles: Vec<Candle>,
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
    };
    let router = Router::new()
        .route("/api/health", get(health))
        .route("/api/state", get(get_state))
        .route("/api/history", get(get_history))
        .route("/api/analysis", get(get_analysis))
        .route("/api/ai/analyze", post(ai_analyze))
        .route("/api/backtest", post(run_backtest))
        .route("/api/config", put(update_config))
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
        fill_model: FillModel::CandleRangeTouch,
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
    Ok(Json(engine.snapshot(Utc::now())))
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
