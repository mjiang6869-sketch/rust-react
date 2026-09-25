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
use domain::{ContractKind, FeeSchedule, FeeSource, Instrument, Precision, RiskLimits};
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
