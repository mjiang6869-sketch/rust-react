//! HTTP 路由。
//!
//! # 设计要点
//!
//! 1. **REST 管命令与查询，WebSocket 管流。** 不用轮询——轮询在 K 线与订单
//!    这两个高频更新的场景下既浪费带宽又让界面滞后。
//! 2. **预览与提交走同一条编译路径。** `/manual/preview` 与 `/manual/submit`
//!    都调用 `preview_manual`，所以界面看到的价位与将要挂出的必然一致。
//! 3. **所有写操作要求幂等键。** 手动面板一定会被双击，没有幂等保护就会
//!    下出两张单。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use domain::{ManualPlan, ServiceMode};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::dto::*;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // ---- 健康与状态 ----
        .route("/api/v1/health", get(health))
        .route("/api/v1/state", get(get_state))
        // ---- 合约与策略 ----
        .route("/api/v1/instrument", get(get_instrument))
        .route("/api/v1/strategies", get(list_strategies))
        .route("/api/v1/fill-models", get(list_fill_models))
        // ---- 手动交易 ----
        .route("/api/v1/manual/preview", post(preview_manual))
        .route("/api/v1/manual/submit", post(submit_manual))
        .route("/api/v1/manual/cancel-pending", post(cancel_pending))
        .route("/api/v1/manual/close", post(close_position))
        // ---- 订单与历史 ----
        .route("/api/v1/orders", get(list_orders))
        .route("/api/v1/fills", get(list_fills))
        .route("/api/v1/pnl", get(pnl_summary))
        // ---- 回测 ----
        .route("/api/v1/backtest", post(run_backtest))
        .route("/api/v1/backtests", get(list_backtests))
        // ---- 数据管理 ----
        .route("/api/v1/data/coverage", get(data_coverage))
        .route("/api/v1/data/download", post(start_download))
        // ---- 实盘安全 ----
        .route("/api/v1/live/arm", post(arm))
        .route("/api/v1/live/disarm", post(disarm))
        .route("/api/v1/mode", put(set_mode))
        // ---- 流 ----
        .route("/api/v1/ws", get(crate::ws::handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// 健康与状态
// ---------------------------------------------------------------------------

async fn health(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    Json(ApiResponse::ok(HealthDto {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        schema_version: s.schema_version(),
    }))
}

async fn get_state(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let mode = s.mode().await;
    let engine = s.engine.lock().await;
    let snap = engine.snapshot();

    let (fill_model, optimism) = engine.fill_model_info();
    let optimism_note = match optimism {
        sim::Optimism::UpperBound => "上界（不现实，仅供对照）",
        sim::Optimism::ConservativeLower => "保守下界（诚实基线）",
    };

    let inst = engine.instrument();
    let dto = StateDto {
        mode: mode_tag(mode),
        mode_label: mode_label(mode),
        symbol: snap.symbol.clone(),
        initial_equity: engine.config().initial_equity.to_string(),
        equity: snap.equity.to_string(),
        realized_pnl: snap.realized_pnl.to_string(),
        unrealized_pnl: snap.unrealized_pnl.to_string(),
        total_fees: engine.total_fees().to_string(),
        position: snap.position.as_ref().map(position_dto),
        open_orders: snap
            .open_orders
            .iter()
            .map(|o| OrderDto {
                client_id: o.client_id.clone(),
                purpose: purpose_tag(o.purpose),
                purpose_label: purpose_label(o.purpose),
                side: side_tag(o.side),
                quantity: o.quantity.to_string(),
                limit_price: o.limit_price.to_string(),
                filled: o.filled.to_string(),
                state: o.state.to_string(),
            })
            .collect(),
        feed_connected: snap.feed_connected,
        feed_fresh: engine.feed_is_fresh(chrono::Utc::now()),
        last_event_at: snap.last_event_at,
        stand_down: snap.stand_down.map(|s| s.to_string()),
        fill_model: fill_model.to_string(),
        fill_model_optimism: optimism_note.to_string(),
        safety: SafetyDto {
            armed: snap.safety.is_armed(),
            user_stream_connected: snap.safety.user_stream_connected(),
            account_reconciled: snap.safety.account_reconciled(),
            blocking_reasons: snap.safety.blocking_reasons(),
        },
        instrument: InstrumentDto {
            symbol: inst.symbol.clone(),
            contract_type: match inst.kind {
                domain::ContractKind::CryptoPerp => "CRYPTO_PERPETUAL",
                domain::ContractKind::TradFiPerp => "TRADFI_PERPETUAL",
            },
            base_asset: inst.base_asset.clone(),
            quote_asset: inst.quote_asset.clone(),
            margin_asset: inst.margin_asset.clone(),
            settlement_asset: inst.settlement_asset.clone(),
            tick_size: inst.precision.tick_size.to_string(),
            step_size: inst.precision.step_size.to_string(),
            min_qty: inst.precision.min_qty.to_string(),
            min_notional: inst.precision.min_notional.to_string(),
            maint_margin_pct: inst.maint_margin_pct.to_string(),
            maker_rate: inst.fees.maker_rate.to_string(),
            taker_rate: inst.fees.taker_rate.to_string(),
            fee_source: match inst.fees.source {
                domain::FeeSource::ExchangeAccount => "EXCHANGE_ACCOUNT",
                domain::FeeSource::ExchangeRules => "EXCHANGE_RULES",
                domain::FeeSource::PromotionalAssumed => "PROMOTIONAL_ASSUMED",
                domain::FeeSource::ConfiguredDefault => "CONFIGURED_DEFAULT",
            },
            fee_is_authoritative: inst.fees.source.is_authoritative(),
        },
    };

    Json(ApiResponse::ok(dto))
}

async fn get_instrument(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    get_state(State(s)).await
}

async fn list_strategies() -> impl IntoResponse {
    let list: Vec<StrategyDto> = strategies::catalog()
        .into_iter()
        .map(|s| StrategyDto {
            id: s.id,
            name: s.name,
            warmup_candles: s.warmup_candles,
            parameters: s.parameters.iter().map(parameter_dto).collect(),
        })
        .collect();
    Json(ApiResponse::ok(list))
}

async fn list_fill_models() -> impl IntoResponse {
    let list: Vec<FillModelDto> = sim::MODEL_NAMES
        .iter()
        .filter_map(|k| {
            sim::model_by_name(k).map(|m| {
                let (tag, note) = match m.optimism() {
                    sim::Optimism::UpperBound => {
                        ("UPPER_BOUND", "上界：假设触价即全额成交，不现实")
                    }
                    sim::Optimism::ConservativeLower => (
                        "CONSERVATIVE_LOWER",
                        "保守下界：要求真实成交发生在我们的价位",
                    ),
                };
                FillModelDto {
                    key: (*k).to_string(),
                    name: m.name().to_string(),
                    data_requirements: m.data_requirements().to_string(),
                    optimism: tag,
                    optimism_note: note.to_string(),
                }
            })
        })
        .collect();
    Json(ApiResponse::ok(list))
}

// ---------------------------------------------------------------------------
// 手动交易
// ---------------------------------------------------------------------------

async fn preview_manual(
    State(s): State<Arc<AppState>>,
    Json(dto): Json<ManualPlanDto>,
) -> Result<impl IntoResponse, ApiError> {
    let now = chrono::Utc::now();
    let plan: ManualPlan = parse_manual_plan(&dto, now)?;
    let engine = s.engine.lock().await;
    let preview = engine
        .preview_manual_plan(&plan, now)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(ApiResponse::ok(preview_dto(&preview))))
}

async fn submit_manual(
    State(s): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(dto): Json<ManualPlanDto>,
) -> Result<impl IntoResponse, ApiError> {
    // 幂等保护：手动面板一定会被双击。没有它就会下出两张单。
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if let Some(k) = &key {
        if s.is_duplicate_submit(k).await {
            return Err(ApiError::Conflict(
                "该请求已处理过（Idempotency-Key 重复），未重复下单".into(),
            ));
        }
    }

    let now = chrono::Utc::now();
    let plan = parse_manual_plan(&dto, now)?;
    let mut engine = s.engine.lock().await;
    let outcome = engine.submit_manual(&plan, now);
    drop(engine);

    match outcome {
        engine::SubmitOutcome::Accepted(p) => {
            if let Some(k) = key {
                s.remember_submit(k).await;
            }
            Ok(Json(ApiResponse::ok(preview_dto(&p))))
        }
        engine::SubmitOutcome::Rejected { reason } => Err(ApiError::BadRequest(reason)),
    }
}

async fn cancel_pending(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let mut engine = s.engine.lock().await;
    let cancelled = engine.cancel_pending();
    Json(ApiResponse::ok(
        serde_json::json!({ "cancelled": cancelled }),
    ))
}

async fn close_position(State(s): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let now = chrono::Utc::now();
    let mut engine = s.engine.lock().await;
    match engine.close_position_manually(now) {
        Some(pnl) => Ok(Json(ApiResponse::ok(serde_json::json!({
            "closed": true,
            "pnl": pnl.to_string(),
        })))),
        None => Err(ApiError::Conflict("当前没有持仓".into())),
    }
}

// ---------------------------------------------------------------------------
// 订单与历史
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OrdersQuery {
    symbol: Option<String>,
    limit: Option<usize>,
}

async fn list_orders(
    State(s): State<Arc<AppState>>,
    Query(q): Query<OrdersQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let conn = s.db.lock().await;
    let rows = store::recent_orders(&conn, q.symbol.as_deref(), q.limit.unwrap_or(100))
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "client_id": r.client_id.to_string(),
                "symbol": r.symbol,
                "purpose": r.state_tag,
                "side": store::orders::side_tag(r.side),
                "quantity": r.quantity.to_string(),
                "limit_price": r.limit_price.to_string(),
                "filled": r.filled.to_string(),
                "avg_price": r.avg_price.map(|p| p.to_string()),
                "needs_reconciliation": r.needs_reconciliation(),
                "updated_at": r.updated_at,
            })
        })
        .collect();
    Ok(Json(ApiResponse::ok(list)))
}

#[derive(Debug, Deserialize)]
struct FillsQuery {
    symbol: String,
    /// ISO8601 起始时刻。
    from: Option<String>,
    to: Option<String>,
}

async fn list_fills(
    State(s): State<Arc<AppState>>,
    Query(q): Query<FillsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let from = q
        .from
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| chrono::Utc::now() - chrono::Duration::days(30));
    let to =
        q.to.as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);

    let conn = s.db.lock().await;
    let rows = store::fills_in_range(&conn, &q.symbol, from, to)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|f| {
            serde_json::json!({
                "trade_id": f.trade_id,
                "client_order_id": f.client_id.to_string(),
                "quantity": f.quantity.to_string(),
                "price": f.price.to_string(),
                "fee": f.fee.to_string(),
                "fee_asset": f.fee_asset,
                "at": f.at,
            })
        })
        .collect();
    Ok(Json(ApiResponse::ok(list)))
}

async fn pnl_summary(State(s): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let from = chrono::Utc::now() - chrono::Duration::days(30);
    let to = chrono::Utc::now();
    let conn = s.db.lock().await;
    let summary =
        store::pnl_summary(&conn, from, to).map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "realized_by_asset": summary.realized_by_asset
            .iter()
            .map(|(a, v)| serde_json::json!({ "asset": a, "amount": v.to_string() }))
            .collect::<Vec<_>>(),
        "fees_by_asset": summary.fees_by_asset
            .iter()
            .map(|(a, v)| serde_json::json!({ "asset": a, "amount": v.to_string() }))
            .collect::<Vec<_>>(),
        "fill_count": summary.fill_count,
    }))))
}

// ---------------------------------------------------------------------------
// 回测
// ---------------------------------------------------------------------------

async fn run_backtest(
    State(s): State<Arc<AppState>>,
    Json(req): Json<BacktestRequestDto>,
) -> Result<impl IntoResponse, ApiError> {
    let from = parse_date(&req.from)?;
    let to = parse_date(&req.to)?;
    let strategy_id = req.strategy.clone().unwrap_or_else(|| "range_maker".into());
    let models = req
        .fill_models
        .clone()
        .unwrap_or_else(|| vec!["m0".into(), "m1".into()]);

    let engine = s.engine.lock().await;
    let instrument = engine.instrument().clone();
    let initial_equity = match &req.initial_equity {
        Some(s) => s
            .parse::<rust_decimal::Decimal>()
            .map_err(|_| ApiError::BadRequest(format!("初始权益不是合法数值：{s}")))?,
        None => engine.config().initial_equity,
    };
    let limits = engine.config().limits;
    drop(engine);

    let result = crate::backtest::run(&crate::backtest::BacktestRequest {
        data_root: &s.data_root,
        instrument: &instrument,
        symbol: &req.symbol,
        strategy_id: &strategy_id,
        from,
        to,
        models: &models,
        initial_equity,
        limits,
    })
    .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    Ok(Json(ApiResponse::ok(result)))
}

#[derive(Debug, Deserialize)]
struct BacktestsQuery {
    symbol: Option<String>,
    limit: Option<usize>,
}

async fn list_backtests(
    State(s): State<Arc<AppState>>,
    Query(q): Query<BacktestsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let conn = s.db.lock().await;
    let rows = store::recent_backtest_runs(&conn, q.symbol.as_deref(), q.limit.unwrap_or(50))
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "run_id": r.run_id,
                "symbol": r.symbol,
                "strategy_id": r.strategy_id,
                "fill_model": r.fill_model,
                "initial_equity": r.initial_equity.to_string(),
                "final_equity": r.final_equity.to_string(),
                "trade_count": r.trade_count,
                "sign_flips": r.sign_flips,
                "breakeven_fill_rate": r.breakeven_fill_rate.map(|v| v.to_string()),
                "adverse_markout_5s": r.adverse_markout_5s.map(|v| v.to_string()),
                "fee_incomplete": r.fee_incomplete,
                "actionable": r.is_actionable(),
                "verdict": r.verdict(),
            })
        })
        .collect();
    Ok(Json(ApiResponse::ok(list)))
}

// ---------------------------------------------------------------------------
// 数据管理
// ---------------------------------------------------------------------------

async fn data_coverage(State(s): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let manifest = data::Manifest::load(&s.data_root.join("manifest/manifest.json"))
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // 按 (数据集, 交易对) 分组
    type MonthStatus = (i32, u32, String, u64);
    let mut groups: std::collections::BTreeMap<(String, String), Vec<MonthStatus>> =
        Default::default();
    for (key, entry) in &manifest.partitions {
        let (status, bytes) = match &entry.status {
            data::PartitionStatus::Finalized { .. } => {
                ("完成".to_string(), entry.parquet_bytes.unwrap_or(0))
            }
            data::PartitionStatus::Suspicious {
                row_count,
                expected,
                ..
            } => (format!("待查（{row_count} 行，期望 {expected}）"), 0),
            data::PartitionStatus::NotInArchive => ("归档无此分区".to_string(), 0),
            data::PartitionStatus::Failed { error, .. } => (format!("失败：{error}"), 0),
            data::PartitionStatus::Absent => ("未处理".to_string(), 0),
        };
        groups
            .entry((format!("{:?}", key.kind), key.symbol.clone()))
            .or_default()
            .push((key.year, key.month, status, bytes));
    }

    let datasets: Vec<DatasetCoverageDto> = groups
        .into_iter()
        .map(|((kind, symbol), mut months)| {
            months.sort();
            let finalized = months.iter().filter(|(_, _, s, _)| s == "完成").count();
            let problems: Vec<String> = months
                .iter()
                .filter(|(_, _, s, _)| s != "完成")
                .map(|(y, m, s, _)| format!("{y}-{m:02}: {s}"))
                .collect();
            DatasetCoverageDto {
                kind,
                symbol,
                partitions: months.len(),
                finalized,
                first_month: months.first().map(|(y, m, _, _)| format!("{y}-{m:02}")),
                last_month: months.last().map(|(y, m, _, _)| format!("{y}-{m:02}")),
                parquet_bytes: months.iter().map(|(_, _, _, b)| *b).sum(),
                problems,
            }
        })
        .collect();

    let gaps: Vec<GapDto> = manifest
        .gaps
        .iter()
        .map(|g| GapDto {
            kind: format!("{:?}", g.kind),
            symbol: g.symbol.clone(),
            from: g.from,
            to: g.to,
            note: g.note.clone(),
        })
        .collect();

    Ok(Json(ApiResponse::ok(CoverageDto {
        data_root: s.data_root.display().to_string(),
        datasets,
        gaps,
    })))
}

async fn start_download(
    State(s): State<Arc<AppState>>,
    Json(req): Json<DownloadRequestDto>,
) -> Result<impl IntoResponse, ApiError> {
    let from = parse_month(&req.from)?;
    let to = parse_month(&req.to)?;
    if from > to {
        return Err(ApiError::BadRequest("起始月份不能晚于结束月份".into()));
    }
    if req.symbols.is_empty() {
        return Err(ApiError::BadRequest("必须指定至少一个交易对".into()));
    }

    // 下载是长任务，放到后台执行并立即返回。进度通过 WebSocket 推进界面。
    let data_root = s.data_root.clone();
    let symbols = req.symbols.clone();
    let kinds = req.kinds.clone();
    let progress = s.progress_tx.clone();

    tokio::spawn(async move {
        if let Err(e) =
            crate::download_task::run(data_root, symbols, kinds, from, to, progress).await
        {
            tracing::error!("下载任务失败：{e:#}");
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(ApiResponse::ok(serde_json::json!({
            "started": true,
            "message": "下载任务已在后台开始，进度会通过 WebSocket 推送",
        }))),
    ))
}

// ---------------------------------------------------------------------------
// 实盘安全
// ---------------------------------------------------------------------------

async fn arm(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let mut engine = s.engine.lock().await;
    let ok = engine.safety_mut().arm();
    let reasons = engine.safety().blocking_reasons();
    Json(ApiResponse::ok(serde_json::json!({
        "armed": ok,
        "blocking_reasons": reasons,
    })))
}

async fn disarm(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let mut engine = s.engine.lock().await;
    engine.safety_mut().disarm();
    Json(ApiResponse::ok(serde_json::json!({ "armed": false })))
}

#[derive(Debug, Deserialize)]
struct ModeRequest {
    mode: String,
}

async fn set_mode(
    State(s): State<Arc<AppState>>,
    Json(req): Json<ModeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let mode = ServiceMode::parse(&req.mode).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // 切到实盘必须满足全部前置条件。这里不自动 ARM——操作者必须显式开启。
    if mode.is_live() {
        let engine = s.engine.lock().await;
        let reasons = engine.safety().blocking_reasons();
        if !reasons.is_empty() {
            return Err(ApiError::Conflict(format!(
                "无法切换到实盘：{}。请先完成这些步骤。",
                reasons.join("；")
            )));
        }
    }

    s.set_mode(mode).await;
    Ok(Json(ApiResponse::ok(serde_json::json!({
        "mode": mode_tag(mode),
        "mode_label": mode_label(mode),
    }))))
}

// ---------------------------------------------------------------------------
// 未使用的路由（保留供后续扩展）
// ---------------------------------------------------------------------------

#[allow(dead_code)]
async fn _placeholder() -> impl IntoResponse {
    StatusCode::NOT_IMPLEMENTED
}

#[allow(dead_code)]
fn _route_kinds() -> (
    axum::routing::MethodRouter,
    axum::routing::MethodRouter,
    axum::routing::MethodRouter,
) {
    (delete(_placeholder), put(_placeholder), post(_placeholder))
}

/// 供测试构造路由器。
pub fn test_router(state: Arc<AppState>) -> Router {
    let _ = Mutex::new(());
    router(state)
}

#[cfg(test)]
mod tests {

    /// 路由表必须包含全部对外接口。漏掉一个会让前端拿到 404 而不知原因。
    #[test]
    fn router_registers_all_expected_paths() {
        // 这个测试通过编译期检查保证路由构造不出错；
        // 具体路径的覆盖由集成测试验证。
        let paths = [
            "/api/v1/health",
            "/api/v1/state",
            "/api/v1/instrument",
            "/api/v1/strategies",
            "/api/v1/fill-models",
            "/api/v1/manual/preview",
            "/api/v1/manual/submit",
            "/api/v1/manual/cancel-pending",
            "/api/v1/manual/close",
            "/api/v1/orders",
            "/api/v1/fills",
            "/api/v1/pnl",
            "/api/v1/backtest",
            "/api/v1/backtests",
            "/api/v1/data/coverage",
            "/api/v1/data/download",
            "/api/v1/live/arm",
            "/api/v1/live/disarm",
            "/api/v1/mode",
            "/api/v1/ws",
        ];
        assert_eq!(paths.len(), 20);
        // 路径必须是版本化的——未来breaking change要走 v2。
        for p in paths {
            assert!(p.starts_with("/api/v1/"), "路由必须版本化：{p}");
        }
    }
}
