//! API 集成测试。
//!
//! 这些测试跑**真实的路由器**（用 `tower::ServiceExt::oneshot`），而不是直接
//! 调用处理函数。差别很重要：路由注册错误、提取器配置错误、序列化问题只有
//! 跑真实路由才能发现。
//!
//! 不涉及网络：`oneshot` 直接调用 service，不起 TCP 监听。

use std::path::PathBuf;
use std::sync::Arc;

use api::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use domain::{
    ContractKind, FeeSchedule, FeeSource, Instrument, Precision, RiskLimits, ServiceMode,
};
use engine::{EngineConfig, PaperEngine};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tower::ServiceExt;

fn test_state() -> Arc<AppState> {
    let conn = rusqlite::Connection::open_in_memory().expect("内存数据库");
    store::configure(&conn).expect("配置");
    store::migrate(&conn).expect("迁移");

    let instrument = Instrument {
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
    };

    AppState::new(
        PaperEngine::new(EngineConfig {
            instrument,
            limits: RiskLimits::default(),
            initial_equity: dec!(10000),
            assumed_latency_ms: 100,
            max_candles: 120,
            max_staleness_secs: 15,
        }),
        conn,
        PathBuf::from("/tmp/rc-api-test"),
    )
}

async fn get(state: &Arc<AppState>, path: &str) -> (StatusCode, serde_json::Value) {
    let app = api::router(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("构造请求"),
        )
        .await
        .expect("服务调用");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("读取响应");
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn post_json(
    state: &Arc<AppState>,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    post_json_with_key(state, path, body, None).await
}

async fn post_json_with_key(
    state: &Arc<AppState>,
    path: &str,
    body: serde_json::Value,
    idempotency_key: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let app = api::router(state.clone());
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("Content-Type", "application/json");
    if let Some(k) = idempotency_key {
        builder = builder.header("Idempotency-Key", k);
    }
    let res = app
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("构造请求"),
        )
        .await
        .expect("服务调用");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("读取响应");
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn put_json(
    state: &Arc<AppState>,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = api::router(state.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(path)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("构造请求"),
        )
        .await
        .expect("服务调用");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("读取响应");
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// 健康检查必须返回 schema 版本——迁移出问题时这是第一手线索。
#[tokio::test]
async fn health_reports_schema_version() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["ok"], true);
    assert_eq!(body["data"]["schema_version"], store::CURRENT_VERSION);
}

/// 引擎状态必须包含界面渲染所需的全部字段。
#[tokio::test]
async fn state_exposes_engine_snapshot() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/state").await;
    assert_eq!(status, StatusCode::OK);

    let d = &body["data"];
    assert_eq!(d["mode"], "PAPER", "默认必须是模拟盘");
    assert_eq!(d["mode_label"], "模拟盘");
    assert_eq!(d["symbol"], "ETHUSDC");
    assert_eq!(d["equity"], "10000");
    assert!(d["position"].is_null(), "初始无持仓");

    // 合约信息
    assert_eq!(d["instrument"]["settlement_asset"], "USDC");
    assert_eq!(d["instrument"]["margin_asset"], "USDC");
    assert_eq!(d["instrument"]["maint_margin_pct"], "2.5");

    // 费率来源必须标注为未对账——整个 edge 依赖这个假设
    assert_eq!(d["instrument"]["fee_source"], "PROMOTIONAL_ASSUMED");
    assert_eq!(d["instrument"]["fee_is_authoritative"], false);

    // 成交模型必须暴露，用户需要知道结论建立在哪种假设上
    assert_eq!(d["fill_model"], "M1_trade_through_queue");
    assert!(d["fill_model_optimism"].as_str().unwrap().contains("保守"));

    // 安全闸门默认未武装，且列出全部阻止原因
    assert_eq!(d["safety"]["armed"], false);
    let reasons = d["safety"]["blocking_reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 3, "三项前置条件都要列出：{reasons:?}");
}

#[tokio::test]
async fn overview_uses_current_paper_snapshot_without_inventing_history() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/overview").await;
    assert_eq!(status, StatusCode::OK);
    let data = &body["data"];
    assert_eq!(data["source"], "paper_account_snapshots");
    assert_eq!(data["settlement_asset"], "USDC");
    assert_eq!(data["equity"], "10000");
    assert_eq!(data["cumulative_pnl"], "0");
    assert_eq!(data["curve"].as_array().unwrap().len(), 1);
    assert!(data["estimated_month_pnl"].is_null());
    assert!(data["estimated_annualized_pct"].is_null());

    s.set_mode(ServiceMode::Live).await;
    let (status, _) = get(&s, "/api/v1/overview").await;
    assert_eq!(status, StatusCode::CONFLICT, "实盘不能展示模拟盘收益图");
}

#[tokio::test]
async fn strategies_list_includes_parameter_documentation() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/strategies").await;
    assert_eq!(status, StatusCode::OK);
    let list = body["data"].as_array().unwrap();
    assert!(!list.is_empty());

    let first = &list[0];
    assert!(!first["name"].as_str().unwrap().is_empty());
    let params = first["parameters"].as_array().unwrap();
    assert!(!params.is_empty(), "策略必须提供参数说明");

    // 每个参数都要有用户能读懂的标签与说明——前端靠它解释参数含义
    for p in params {
        assert!(
            !p["label"].as_str().unwrap().is_empty(),
            "参数缺少标签：{p}"
        );
        assert!(
            !p["description"].as_str().unwrap().is_empty(),
            "参数缺少说明：{p}"
        );
        assert!(
            p["display_as_percent"].is_boolean(),
            "百分比标记必须存在，否则前端会显示错 100 倍：{p}"
        );
    }
}

/// 百分比参数必须被标记，否则前端会把 0.1 显示成 0.1% 而非 10%。
#[tokio::test]
async fn percent_parameters_are_flagged() {
    let s = test_state();
    let (_, body) = get(&s, "/api/v1/strategies").await;
    let params = body["data"][0]["parameters"].as_array().unwrap();

    let pct_param = params
        .iter()
        .find(|p| p["unit"] == "%")
        .expect("应有百分比参数");
    assert_eq!(pct_param["display_as_percent"], true);

    let bp_param = params.iter().find(|p| p["unit"] == "基点");
    if let Some(p) = bp_param {
        assert_eq!(p["display_as_percent"], false, "基点不是百分比");
    }
}

#[tokio::test]
async fn fill_models_are_documented_with_optimism() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/fill-models").await;
    assert_eq!(status, StatusCode::OK);
    let list = body["data"].as_array().unwrap();
    assert_eq!(list.len(), 2, "应有 M0 与 M1");

    let m0 = list.iter().find(|m| m["key"] == "m0").expect("应有 m0");
    let m1 = list.iter().find(|m| m["key"] == "m1").expect("应有 m1");
    assert_eq!(m0["optimism"], "UPPER_BOUND");
    assert_eq!(m1["optimism"], "CONSERVATIVE_LOWER");
    assert!(m0["optimism_note"].as_str().unwrap().contains("不现实"));
}

// ---------------------------------------------------------------------------
// 手动下单
// ---------------------------------------------------------------------------

fn valid_plan() -> serde_json::Value {
    serde_json::json!({
        "symbol": "ETHUSDC",
        "side": "BUY",
        "entry": "3200",
        "quantity": "0.1",
        "leverage": "3",
        "stop": "3192",
        "take_profit": [
            { "pct": "0.0025", "fraction": "0.5" },
            { "pct": "0.005", "fraction": "0.5" }
        ],
        "client_ref": "manual"
    })
}

/// 预览必须返回量化后的价位——这就是将要挂出的价位。
#[tokio::test]
async fn preview_returns_quantized_prices() {
    let s = test_state();
    let (status, body) = post_json(&s, "/api/v1/manual/preview", valid_plan()).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let d = &body["data"];
    assert_eq!(d["entry"], "3200");
    assert_eq!(d["stop"], "3192");
    assert_eq!(d["quantity"], "0.1");
    assert_eq!(d["accepted"], true, "{d}");

    let tps = d["take_profits"].as_array().unwrap();
    assert_eq!(tps.len(), 2);
    // 各档价格递增（平多止盈向上取整）
    assert!(tps[0]["price"].as_str().unwrap() < tps[1]["price"].as_str().unwrap());
    // 数量合计等于下总量。用 Decimal 而非浮点——这是项目的硬约束，
    // 而且 clippy.toml 在编译期就禁止了 f32/f64 承载交易数值。
    let total: Decimal = tps
        .iter()
        .map(|t| t["quantity"].as_str().unwrap().parse::<Decimal>().unwrap())
        .sum();
    assert_eq!(total, dec!(0.1), "各档合计应等于下总量");
}

/// 距离意图必须沿用保护单规划器的方向量化，旧止损价格请求仍然等价。
#[tokio::test]
async fn preview_stop_distance_matches_explicit_stop_for_both_sides() {
    for (side, raw_stop, expected) in [
        ("BUY", "3192.029925", "3192.02"),
        ("SELL", "3208.030075", "3208.04"),
    ] {
        let s = test_state();
        let mut explicit = valid_plan();
        explicit["side"] = serde_json::json!(side);
        explicit["entry"] = serde_json::json!("3200.03");
        explicit["stop"] = serde_json::json!(raw_stop);
        let mut distance = explicit.clone();
        distance.as_object_mut().unwrap().remove("stop");
        distance["stop_distance_bp"] = serde_json::json!("25");
        let (old_status, old) = post_json(&s, "/api/v1/manual/preview", explicit).await;
        let (status, body) = post_json(&s, "/api/v1/manual/preview", distance).await;
        assert_eq!(old_status, StatusCode::OK, "{old}");
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, old);
        assert_eq!(body["data"]["stop"], expected);
        let (_, state) = get(&s, "/api/v1/state").await;
        assert!(state["data"]["open_orders"].as_array().unwrap().is_empty());
    }
}

/// 预览不能改变引擎状态。
#[tokio::test]
async fn preview_does_not_mutate_state() {
    let s = test_state();
    let (_, before) = get(&s, "/api/v1/state").await;
    let _ = post_json(&s, "/api/v1/manual/preview", valid_plan()).await;
    let (_, after) = get(&s, "/api/v1/state").await;

    assert_eq!(before["data"]["equity"], after["data"]["equity"]);
    assert!(after["data"]["position"].is_null(), "预览不应建立持仓");
    assert!(after["data"]["open_orders"].as_array().unwrap().is_empty());
}

/// 止损方向错误必须被拒绝，且给出可读原因。
#[tokio::test]
async fn preview_rejects_wrong_side_stop() {
    let s = test_state();
    let mut plan = valid_plan();
    plan["stop"] = serde_json::json!("3210"); // 多头止损放在上方

    let (status, body) = post_json(&s, "/api/v1/manual/preview", plan).await;
    assert_eq!(status, StatusCode::OK, "预览本身成功，但内容应被拒绝");

    let d = &body["data"];
    assert_eq!(d["accepted"], false);
    assert!(
        d["reject_reason"].as_str().unwrap().contains("止损"),
        "拒绝原因应指出是止损问题：{d}"
    );
}

/// 各档比例之和超过 100% 必须被拒绝——否则会超卖。
#[tokio::test]
async fn preview_rejects_ladder_over_one_hundred_percent() {
    let s = test_state();
    let mut plan = valid_plan();
    plan["take_profit"] = serde_json::json!([
        { "pct": "0.0025", "fraction": "0.7" },
        { "pct": "0.005", "fraction": "0.7" }
    ]);

    let (status, body) = post_json(&s, "/api/v1/manual/preview", plan).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let msg = body["message"].as_str().unwrap();
    assert!(msg.contains("超过"), "错误应说明原因：{msg}");
}

/// 非法数值必须返回 400 并指出字段名。
#[tokio::test]
async fn preview_rejects_invalid_decimal_with_field_name() {
    let s = test_state();
    let mut plan = valid_plan();
    plan["entry"] = serde_json::json!("abc");

    let (status, body) = post_json(&s, "/api/v1/manual/preview", plan).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("entry"));
}

/// 提交必须先要幂等键。
#[tokio::test]
async fn submit_requires_idempotency_key_to_dedupe() {
    let s = test_state();

    let (status, body) =
        post_json_with_key(&s, "/api/v1/manual/submit", valid_plan(), Some("k1")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["accepted"], true);

    // 同一键重复提交必须被拒绝，而不是下出第二张单
    let (status2, body2) =
        post_json_with_key(&s, "/api/v1/manual/submit", valid_plan(), Some("k1")).await;
    assert_eq!(status2, StatusCode::CONFLICT, "{body2}");
    assert!(
        body2["message"].as_str().unwrap().contains("重复"),
        "错误应说明是重复提交：{body2}"
    );

    // 用不同键则允许（虽然此时已有在途单会被业务规则拒绝）
    let (_, body3) =
        post_json_with_key(&s, "/api/v1/manual/submit", valid_plan(), Some("k2")).await;
    let msg = body3["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("在途") || msg.contains("持仓"),
        "已有在途单时应被业务规则拒绝：{body3}"
    );
}

/// 提交后应能撤单，且撤单后可重新提交。
#[tokio::test]
async fn cancel_pending_then_resubmit() {
    let s = test_state();
    let (_, body) = post_json_with_key(&s, "/api/v1/manual/submit", valid_plan(), Some("k1")).await;
    assert_eq!(body["data"]["accepted"], true);

    let (status, cancelled) =
        post_json(&s, "/api/v1/manual/cancel-pending", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["data"]["cancelled"], true);

    // 再次撤单应返回 false（没有在途单）
    let (_, again) = post_json(&s, "/api/v1/manual/cancel-pending", serde_json::json!({})).await;
    assert_eq!(again["data"]["cancelled"], false);

    // 撤单后可以重新提交
    let (status2, body2) =
        post_json_with_key(&s, "/api/v1/manual/submit", valid_plan(), Some("k3")).await;
    assert_eq!(status2, StatusCode::OK, "{body2}");
    assert_eq!(body2["data"]["accepted"], true);
}

/// 无持仓时平仓应返回冲突而非成功。
#[tokio::test]
async fn close_without_position_is_conflict() {
    let s = test_state();
    let (status, body) = post_json(&s, "/api/v1/manual/close", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["message"].as_str().unwrap().contains("没有持仓"),
        "{body}"
    );
}

/// 手动下的在途开仓单必须在状态里标记来源与可撤销——界面靠这两个字段
/// 决定要不要显示撤单按钮，以及区分这是手动单还是自动化做市挂的单。
#[tokio::test]
async fn manual_open_order_is_tagged_manual_and_cancellable() {
    let s = test_state();
    let (_, body) = post_json_with_key(&s, "/api/v1/manual/submit", valid_plan(), Some("k1")).await;
    assert_eq!(body["data"]["accepted"], true, "{body}");

    let (_, state) = get(&s, "/api/v1/state").await;
    let orders = state["data"]["open_orders"].as_array().unwrap();
    assert_eq!(orders.len(), 1, "{orders:?}");
    let order = &orders[0];
    assert_eq!(order["purpose"], "ENTRY");
    assert_eq!(order["source"], "MANUAL");
    assert_eq!(order["cancellable"], true, "{order}");
}

/// 撤单必须支持按 `client_id` 精确撤销：错误的 ID 应是冲突（409），
/// 正确的 ID 应成功并回显来源。
#[tokio::test]
async fn cancel_pending_by_client_id() {
    let s = test_state();
    let (_, body) = post_json_with_key(&s, "/api/v1/manual/submit", valid_plan(), Some("k1")).await;
    assert_eq!(body["data"]["accepted"], true, "{body}");

    let (_, state) = get(&s, "/api/v1/state").await;
    let client_id = state["data"]["open_orders"][0]["client_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = post_json(
        &s,
        "/api/v1/manual/cancel-pending?client_id=nope",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = post_json(
        &s,
        &format!("/api/v1/manual/cancel-pending?client_id={client_id}"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["cancelled"], true);
    assert_eq!(body["data"]["source"], "MANUAL");
}

// ---------------------------------------------------------------------------
// 自动化做市
// ---------------------------------------------------------------------------

/// 一份合法的白名单参数，供各测试按需覆盖单个字段。
fn default_auto_maker_params() -> serde_json::Value {
    serde_json::json!({
        "lookback": "60",
        "take_profit_bp": "4",
        "stop_buffer_bp": "2",
        "side_mode": "LONG_ONLY",
        "equity_pct": "0.1",
        "leverage": "3",
        "valid_minutes": "2",
    })
}

/// 路由必须真的注册：GET 200、非法 PUT 400，两者都不能是 404。
#[tokio::test]
async fn auto_maker_routes_are_registered() {
    let s = test_state();

    let (status, body) = get(&s, "/api/v1/auto-maker").await;
    assert_ne!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "bogus": true }),
    )
    .await;
    assert_ne!(status, StatusCode::NOT_FOUND, "{body}");
    // `Json2` 保留 axum 对该请求体的原始判定：字段不匹配是
    // `JsonRejection::JsonDataError`，映射到 422（语法合法但语义不对），
    // 不是语法错误的 400——这与仓库里其它端点的既有行为一致。
    assert!(status.is_client_error(), "{body}");
}

/// 默认必须是关闭状态，且带上文档化的可编辑字段说明。
#[tokio::test]
async fn auto_maker_default_is_disabled_with_documented_fields() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/auto-maker").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let d = &body["data"];
    assert_eq!(d["enabled"], false);
    assert_eq!(d["status"], "DISABLED");
    assert!(
        d["status_label"].as_str().unwrap().contains("未启用"),
        "{d}"
    );

    let fields = d["fields"].as_array().unwrap();
    assert!(!fields.is_empty(), "可编辑字段说明不能为空");
    let equity_pct = fields
        .iter()
        .find(|f| f["key"] == "equity_pct")
        .expect("应有 equity_pct 字段说明");
    assert_eq!(
        equity_pct["display_as_percent"], true,
        "仓位比例是百分比，前端要乘 100 显示"
    );

    assert_eq!(d["params"]["lookback"], "60", "默认回看根数");
}

/// 非法的 `lookback` 必须报错，且指出是哪个字段。
#[tokio::test]
async fn auto_maker_put_rejects_non_numeric_lookback() {
    let s = test_state();
    let mut params = default_auto_maker_params();
    params["lookback"] = serde_json::json!("abc");

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "enabled": true, "params": params }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("lookback"),
        "{body}"
    );
}

/// 越界的 `take_profit_bp` 由引擎的 `RangeMakerParams::validate` 拒绝，
/// 错误消息必须用面向用户的中文标签（"止盈距离"），不是内部字段名。
#[tokio::test]
async fn auto_maker_put_rejects_out_of_range_take_profit_bp() {
    let s = test_state();
    let mut params = default_auto_maker_params();
    params["take_profit_bp"] = serde_json::json!("60");

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "enabled": true, "params": params }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("止盈距离"),
        "{body}"
    );
}

/// `lookback` 即便落在策略自身的范围内，也不能超过引擎的 K 线窗口上限
/// （测试引擎配置的是 120 根）——否则策略会引用一段引擎根本没留着的历史。
#[tokio::test]
async fn auto_maker_put_rejects_lookback_over_engine_window() {
    let s = test_state();
    let mut params = default_auto_maker_params();
    params["lookback"] = serde_json::json!("200");

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "enabled": true, "params": params }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("120"), "{body}");
}

/// 白名单之外的字段必须被拒绝，不能被静默忽略。
#[tokio::test]
async fn auto_maker_put_rejects_unknown_param_field() {
    let s = test_state();
    let mut params = default_auto_maker_params();
    params["trailing_bp"] = serde_json::json!("10");

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "enabled": true, "params": params }),
    )
    .await;
    // 未知字段属于结构性错误（`JsonRejection::JsonDataError`），axum 判定为
    // 422，不是 400——见 `auto_maker_routes_are_registered` 的说明。
    assert!(status.is_client_error(), "{body}");
}

/// 合法启用后 `/api/v1/state` 必须能看到 `auto_maker.enabled = true`；
/// 随后关闭也必须成功。
#[tokio::test]
async fn auto_maker_can_be_enabled_and_disabled() {
    let s = test_state();

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "enabled": true, "params": default_auto_maker_params() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["enabled"], true);

    let (_, state) = get(&s, "/api/v1/state").await;
    assert_eq!(state["data"]["auto_maker"]["enabled"], true, "{state}");

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "enabled": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["enabled"], false);
}

/// 实盘模式下：GET 必须显示未启用，PUT 启用必须被拒绝（409），
/// PUT 关闭必须始终成功（关闭没有前提条件）。
#[tokio::test]
async fn auto_maker_cannot_be_enabled_in_live_mode() {
    let s = test_state();
    s.set_mode(ServiceMode::Live).await;

    let (_, body) = get(&s, "/api/v1/auto-maker").await;
    assert_eq!(body["data"]["enabled"], false);

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "enabled": true, "params": default_auto_maker_params() }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = put_json(
        &s,
        "/api/v1/auto-maker",
        serde_json::json!({ "enabled": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// 安全闸门
// ---------------------------------------------------------------------------

/// ARM 必须因前置条件未满足而失败，并说明缺什么。
#[tokio::test]
async fn arm_fails_without_prerequisites() {
    let s = test_state();
    let (status, body) = post_json(&s, "/api/v1/live/arm", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["data"]["armed"], false, "前置条件未满足时不能开启");
    let reasons = body["data"]["blocking_reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 3);
}

#[tokio::test]
async fn disarm_always_succeeds() {
    let s = test_state();
    let (status, body) = post_json(&s, "/api/v1/live/disarm", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["armed"], false);
}

/// 切到实盘必须因前置条件未满足而被拒绝。
#[tokio::test]
async fn cannot_switch_to_live_without_prerequisites() {
    let s = test_state();
    let app = api::router(s.clone());
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/mode")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"mode":"LIVE"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::CONFLICT,
        "前置条件未满足时不能切实盘"
    );
}

/// 模式解析必须严格——模糊匹配是「无提示切到主网」的温床。
#[tokio::test]
async fn mode_parsing_is_strict() {
    let s = test_state();
    for bad in ["livex", "true", "1", "yes"] {
        let app = api::router(s.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/mode")
                    .header("Content-Type", "application/json")
                    .body(Body::from(format!(r#"{{"mode":"{bad}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "模式「{bad}」不应被接受"
        );
    }
}

// ---------------------------------------------------------------------------
// 数据管理
// ---------------------------------------------------------------------------

/// 覆盖查询在台账不存在时也应成功（返回空列表），而不是报错。
#[tokio::test]
async fn coverage_handles_missing_manifest() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/data/coverage").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["datasets"].as_array().unwrap().is_empty());
    assert!(body["data"]["gaps"].as_array().unwrap().is_empty());
}

/// 下载请求参数必须被校验。
#[tokio::test]
async fn download_validates_input() {
    let s = test_state();

    // 空交易对
    let (status, body) = post_json(
        &s,
        "/api/v1/data/download",
        serde_json::json!({ "symbols": [], "kinds": ["klines"], "from": "2026-01", "to": "2026-02" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("交易对"));

    // 月份格式错误
    let (status, body) = post_json(
        &s,
        "/api/v1/data/download",
        serde_json::json!({ "symbols": ["ETHUSDC"], "kinds": ["klines"], "from": "2026", "to": "2026-02" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("YYYY-MM"));

    // 起始晚于结束
    let (status, body) = post_json(
        &s,
        "/api/v1/data/download",
        serde_json::json!({ "symbols": ["ETHUSDC"], "kinds": ["klines"], "from": "2026-05", "to": "2026-02" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("晚于"));
}

/// 未知数据集必须在生成任何任务前就被拒绝，且错误信息列出可用选项。
#[tokio::test]
async fn download_rejects_unknown_kind() {
    let s = test_state();
    let (status, body) = post_json(
        &s,
        "/api/v1/data/download",
        serde_json::json!({ "symbols": ["ETHUSDC"], "kinds": ["bogus"], "from": "2026-01", "to": "2026-02" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("klines"),
        "错误信息应列出可用数据集：{body}"
    );
}

/// 交易对里的路径穿越字符必须在发出任何请求（乃至启动后台任务）之前被拒绝。
#[tokio::test]
async fn download_rejects_symbol_injection() {
    let s = test_state();
    let (status, body) = post_json(
        &s,
        "/api/v1/data/download",
        serde_json::json!({ "symbols": ["../x"], "kinds": ["klines"], "from": "2026-01", "to": "2026-02" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// 没有任务时 GET 必须报 idle。
#[tokio::test]
async fn download_get_reports_idle_without_task() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/data/download").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["state"], "idle");
}

/// 已有下载任务在跑时，再次 POST 必须返回 409，且不能扰乱正在进行的任务。
#[tokio::test]
async fn download_post_conflicts_when_already_running() {
    let s = test_state();
    let _guard = s
        .downloads()
        .try_start(api::state::DownloadJobRequestSnapshot {
            symbols: vec!["ETHUSDC".into()],
            kinds: vec!["klines".into()],
            from: "2026-01".into(),
            to: "2026-02".into(),
        })
        .expect("应能开始第一个任务");

    let (status, body) = post_json(
        &s,
        "/api/v1/data/download",
        serde_json::json!({ "symbols": ["ETHUSDC"], "kinds": ["klines"], "from": "2026-01", "to": "2026-02" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        s.downloads().snapshot().state,
        "running",
        "第一个任务不应受影响"
    );
}

/// 没有任务时取消必须 409，不能假装成功。
#[tokio::test]
async fn download_cancel_conflicts_without_task() {
    let s = test_state();
    let (status, body) = post_json(&s, "/api/v1/data/download/cancel", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

/// 有任务时取消必须成功，且必须真的触发取消令牌，不能只是回报"有任务"。
#[tokio::test]
async fn download_cancel_succeeds_with_running_task() {
    let s = test_state();
    let guard = s
        .downloads()
        .try_start(api::state::DownloadJobRequestSnapshot {
            symbols: vec!["ETHUSDC".into()],
            kinds: vec!["klines".into()],
            from: "2026-01".into(),
            to: "2026-02".into(),
        })
        .expect("应能开始任务");
    let token = guard.cancel_token();

    let (status, body) = post_json(&s, "/api/v1/data/download/cancel", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["cancelled"], true);
    assert!(token.is_cancelled(), "cancel 接口必须真的触发取消令牌");
}

// ---------------------------------------------------------------------------
// 归档范围
// ---------------------------------------------------------------------------

/// 交易对校验必须在发出任何网络请求前拒绝非法输入。
#[tokio::test]
async fn archive_range_rejects_invalid_symbol() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/data/archive-range?symbol=%21%21").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// 非法数据集名称必须 400，且不能触发任何网络请求。
#[tokio::test]
async fn archive_range_rejects_unknown_kind() {
    let s = test_state();
    let (status, body) = get(&s, "/api/v1/data/archive-range?symbol=ETHUSDC&kinds=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// 未知路由返回 404 而非 500。
#[tokio::test]
async fn unknown_route_is_not_found() {
    let s = test_state();
    let (status, _) = get(&s, "/api/v1/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// 所有错误响应都必须是带 code 与 message 的 JSON——
/// 前端靠它显示可读原因。
#[tokio::test]
async fn errors_are_structured_json() {
    let s = test_state();
    let (status, body) = post_json(
        &s,
        "/api/v1/manual/preview",
        serde_json::json!({ "symbol": "ETHUSDC" }),
    )
    .await;
    assert!(status.is_client_error());
    assert_eq!(body["status"], "error");
    assert!(body["code"].is_string(), "错误必须带 code：{body}");
    assert!(body["message"].is_string(), "错误必须带可读消息：{body}");
}

// ---------------------------------------------------------------------------
// 行情
// ---------------------------------------------------------------------------

/// 路由必须真的注册，而不只是出现在某个数组里。
///
/// 用非法交易对请求是刻意的：它在**任何网络调用之前**就被拒绝，所以这个
/// 测试不需要网络，也不依赖币安是否可达。404 说明路由不存在（真正的失败），
/// 400 说明请求到达了处理函数（正是我们要的）。
#[tokio::test]
async fn market_routes_are_registered() {
    let s = test_state();

    for path in [
        "/api/v1/market/klines?symbol=%21%21&interval=1m",
        "/api/v1/market/book?symbol=%21%21",
        // 推送路由在升级为 WebSocket **之前**校验交易对，所以普通 GET 也能
        // 拿到 400——证明它注册了，且非法交易对到不了上游 URL。
        "/api/v1/market/stream?symbol=%21%21",
    ] {
        let (status, body) = get(&s, path).await;
        assert_ne!(status, StatusCode::NOT_FOUND, "{path} 未注册（返回 404）");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} -> {body}");
    }
}

/// 合法交易对但不是 WebSocket 握手：必须被拒绝，且**不能**因此去连上游。
#[tokio::test]
async fn market_stream_requires_websocket_upgrade() {
    let s = test_state();
    let (status, _) = get(&s, "/api/v1/market/stream?symbol=ETHUSDC").await;
    assert_ne!(status, StatusCode::NOT_FOUND);
    assert!(
        status.is_client_error(),
        "非握手请求应是 4xx，实际 {status}"
    );
    assert_eq!(
        s.market_streams().map(|m| m.active_feeds()),
        Some(0),
        "被拒绝的请求不应建立上游连接"
    );
}

/// 周期只接受精确匹配。拼错的周期必须报错，而不是静默变回默认值——
/// 用户选了 4 小时却看到 1 分钟图，是没有提示的错误。
#[tokio::test]
async fn market_rejects_unknown_interval() {
    let s = test_state();
    for bad in ["1min", "2h", "1M", ""] {
        let (status, body) = get(
            &s,
            &format!("/api/v1/market/klines?symbol=ETHUSDC&interval={bad}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "周期「{bad}」: {body}");
        let msg = body["message"].as_str().unwrap();
        assert!(msg.contains("周期"), "错误应说明是周期问题：{msg}");
    }
}

/// 交易对里的注入字符必须在任何网络调用前被拒绝。
#[tokio::test]
async fn market_rejects_symbol_injection() {
    let s = test_state();
    // 注意：`&` 在 URL 里是参数分隔符，所以这里用编码后的 %26
    for bad in ["ETH%26limit%3D1", "ETH%2FUSDC", "%2E%2E%2F%2E%2E"] {
        let (status, body) = get(&s, &format!("/api/v1/market/klines?symbol={bad}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "「{bad}」: {body}");
    }
}

/// 空交易对回退到引擎配置的交易对，而不是报错或发一个空 symbol 的请求。
#[tokio::test]
async fn market_falls_back_to_engine_symbol() {
    let s = test_state();
    // symbol 省略、给出的周期非法 -> 应该先因周期报错，说明 symbol 已回退
    let (status, body) = get(&s, "/api/v1/market/klines?interval=2h").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("周期"), "{body}");
}

#[tokio::test]
async fn market_stream_rejects_invalid_interval_before_connecting() {
    let s = test_state();
    let (status, _) = get(&s, "/api/v1/market/stream?symbol=ETHUSDC&interval=invalid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(s.market_streams().map(|m| m.active_feeds()), Some(0));
}
