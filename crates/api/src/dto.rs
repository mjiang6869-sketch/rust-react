//! 跨进程传输的类型。
//!
//! # 两条硬约束
//!
//! ## 1. Decimal 一律序列化为字符串
//!
//! JavaScript 的 `number` 是 IEEE 754 双精度，`3200.12345678` 这样的价格在
//! 前端会丢精度。而止盈目标是 bp 级，丢精度会让界面显示的价与后端将要挂出的
//! 价不一致——用户看到的不是即将发生的事。
//!
//! 所以所有价格、数量、金额都用字符串传输，前端按字符串显示或交给专门的
//! decimal 库处理。
//!
//! ## 2. 拒绝必须带可读原因
//!
//! 风控拒绝、参数错误、数据缺失都必须返回**面向用户的中文说明**，而不只是
//! 错误码。静默失败是恶劣的失败模式——用户会以为策略不工作而不知道为什么。

use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use domain::{ManualPlan, ManualPreview, PositionView, ServiceMode, Side, StandDownReason};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 统一的 API 响应包装。
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiResponse<T> {
    Ok { data: T },
    Error { code: String, message: String },
}

impl<T> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        ApiResponse::Ok { data }
    }
}

/// 统一的 API 错误。
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("内部错误：{0}")]
    Internal(String),
}

impl ApiError {
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::BadRequest(_) => "bad_request",
            ApiError::NotFound(_) => "not_found",
            ApiError::Conflict(_) => "conflict",
            ApiError::Internal(_) => "internal",
        }
    }

    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = ApiResponse::<()>::Error {
            code: self.code().to_string(),
            message: self.to_string(),
        };
        (self.status(), axum::Json(body)).into_response()
    }
}

/// 把 axum 的提取器拒绝转成统一格式的 JSON。
///
/// # 为什么需要这个
///
/// axum 的 `Json` 提取器在请求体无法反序列化时，默认返回**纯文本**：
///
/// ```text
/// Failed to deserialize the JSON body into the target type: missing field `side`
/// ```
///
/// 前端按 `{status, code, message}` 解析会直接失败（`JSON.parse` 抛错），
/// 用户看到的是「后端返回了非 JSON 响应」而不是「缺少字段 side」。
///
/// 这个提取器统一了格式，并把技术性的错误描述保留在 message 里——虽然它是
/// 英文的，但比「解析失败」有用得多。
pub struct Json2<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for Json2<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = axum::response::Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Json2(v)),
            Err(rejection) => {
                let status = rejection.status();
                let (code, detail) = match rejection {
                    axum::extract::rejection::JsonRejection::JsonDataError(e) => {
                        ("bad_request", format!("请求体字段不匹配：{e}"))
                    }
                    axum::extract::rejection::JsonRejection::JsonSyntaxError(e) => {
                        ("bad_request", format!("请求体不是合法 JSON：{e}"))
                    }
                    axum::extract::rejection::JsonRejection::MissingJsonContentType(_) => (
                        "bad_request",
                        "请求头必须包含 Content-Type: application/json".to_string(),
                    ),
                    axum::extract::rejection::JsonRejection::BytesRejection(e) => {
                        ("bad_request", format!("读取请求体失败：{e}"))
                    }
                    other => ("bad_request", other.body_text()),
                };

                let body = ApiResponse::<()>::Error {
                    code: code.to_string(),
                    message: detail,
                };
                let mut res = (status, axum::Json(body)).into_response();
                res.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
                Err(res)
            }
        }
    }
}

/// 服务健康状态。
#[derive(Debug, Serialize)]
pub struct HealthDto {
    pub ok: bool,
    pub version: String,
    /// 数据库 schema 版本。便于排查迁移问题。
    pub schema_version: i32,
}

/// 引擎状态快照。
#[derive(Debug, Serialize)]
pub struct StateDto {
    /// 当前模式。界面必须显眼展示——模拟盘与实盘不能靠颜色暗示。
    pub mode: &'static str,
    pub mode_label: &'static str,
    pub symbol: String,
    /// 初始权益。
    pub initial_equity: String,
    pub equity: String,
    pub realized_pnl: String,
    pub unrealized_pnl: String,
    /// 累计手续费。
    pub total_fees: String,
    /// 持仓（含各档止盈状态）。
    pub position: Option<PositionDto>,
    pub open_orders: Vec<OrderDto>,
    /// 行情状态。
    pub feed_connected: bool,
    pub feed_fresh: bool,
    pub last_event_at: Option<DateTime<Utc>>,
    /// 策略让位原因。为 `None` 表示策略正常运行。
    pub stand_down: Option<String>,
    /// 成交模型的名称与乐观度。
    ///
    /// **必须展示**：用户需要知道当前结论建立在哪种成交假设上。
    pub fill_model: String,
    pub fill_model_optimism: String,
    /// 实盘安全闸门状态。
    pub safety: SafetyDto,
    /// 合约的关键规则。
    pub instrument: InstrumentDto,
}

/// 合约信息。
#[derive(Debug, Serialize)]
pub struct InstrumentDto {
    pub symbol: String,
    pub contract_type: &'static str,
    pub base_asset: String,
    pub quote_asset: String,
    pub margin_asset: String,
    pub settlement_asset: String,
    pub tick_size: String,
    pub step_size: String,
    pub min_qty: String,
    pub min_notional: String,
    /// 维持保证金率（百分比）。
    ///
    /// 界面用它显示止损距强平的缓冲——这是 maker-only 裸露风险的预警。
    pub maint_margin_pct: String,
    pub maker_rate: String,
    pub taker_rate: String,
    /// 费率来源。非权威来源时界面必须标记结果不可用于决策。
    pub fee_source: &'static str,
    pub fee_is_authoritative: bool,
}

/// 持仓。
#[derive(Debug, Serialize)]
pub struct PositionDto {
    pub symbol: String,
    pub side: &'static str,
    pub side_label: &'static str,
    pub quantity: String,
    pub entry_price: String,
    pub unrealized_pnl: String,
    /// 当前止损价。
    pub stop_price: Option<String>,
    /// 止损已触发但未成交——仓位裸露中，界面必须醒目提示。
    pub stop_triggered: bool,
    /// 各档止盈。
    pub rungs: Vec<RungDto>,
    pub realized_pnl: String,
}

/// 一档止盈。
#[derive(Debug, Serialize)]
pub struct RungDto {
    pub rung: usize,
    /// 档位序号（从 1 开始，给用户看）。
    pub index: usize,
    pub pct: String,
    /// 距入场价的基点。
    pub distance_bp: String,
    pub fraction: String,
    pub price: String,
    pub filled: bool,
}

/// 订单。
#[derive(Debug, Serialize)]
pub struct OrderDto {
    pub client_id: String,
    pub purpose: &'static str,
    pub purpose_label: &'static str,
    pub side: &'static str,
    pub quantity: String,
    pub limit_price: String,
    pub filled: String,
    pub state: String,
}

/// 实盘安全状态。
#[derive(Debug, Serialize)]
pub struct SafetyDto {
    pub armed: bool,
    pub user_stream_connected: bool,
    pub account_reconciled: bool,
    /// 当前阻止交易的原因（面向用户）。
    pub blocking_reasons: Vec<&'static str>,
}

/// 手动下单请求。
#[derive(Debug, Deserialize)]
pub struct ManualPlanDto {
    pub symbol: String,
    pub side: Side,
    /// 入场价（字符串形式的 Decimal）。
    pub entry: String,
    /// 数量。与 `size_pct` 二选一。
    pub quantity: Option<String>,
    /// 按权益比例下单。
    pub size_pct: Option<String>,
    pub leverage: String,
    pub stop: String,
    /// 分批止盈。`None` 表示不分批。
    pub take_profit: Option<Vec<TakeProfitRungDto>>,
    /// 单档止盈百分比（当 `take_profit` 为 `None` 时使用）。
    pub take_profit_pct: Option<String>,
    pub break_even: Option<BreakEvenDto>,
    pub trailing: Option<TrailingDto>,
    /// 挂单超时自动撤销的秒数。
    pub cancel_unfilled_after_secs: Option<i64>,
    /// 客户端引用，用于订单 ID 前缀。
    pub client_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TakeProfitRungDto {
    /// 距入场价的百分比。
    pub pct: String,
    /// 该档平仓比例。
    pub fraction: String,
}

#[derive(Debug, Deserialize)]
pub struct BreakEvenDto {
    pub trigger_r: String,
    pub offset: String,
}

#[derive(Debug, Deserialize)]
pub struct TrailingDto {
    pub distance: String,
    pub activate_at: Option<String>,
}

/// 手动下单预览响应。
#[derive(Debug, Serialize)]
pub struct ManualPreviewDto {
    /// 量化后的入场价——**这就是将要挂出的价**。
    pub entry: String,
    pub stop: String,
    pub quantity: String,
    pub notional: String,
    pub margin_required: String,
    /// 止损距估算强平价的缓冲比例。
    pub liquidation_buffer_pct: Option<String>,
    pub take_profits: Vec<RungPreviewDto>,
    /// 是否通过风控。
    pub accepted: bool,
    /// 拒绝原因（面向用户的中文说明）。
    pub reject_reason: Option<String>,
    /// 警告（不阻断，但必须显示）。
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RungPreviewDto {
    pub rung: usize,
    pub index: usize,
    pub price: String,
    pub quantity: String,
    pub gross_profit: String,
    pub distance_bp: String,
}

/// 策略与参数说明。
#[derive(Debug, Serialize)]
pub struct StrategyDto {
    pub id: String,
    pub name: String,
    pub warmup_candles: usize,
    pub parameters: Vec<ParameterDto>,
}

/// 参数说明。**前端靠它解释每个参数的含义。**
#[derive(Debug, Serialize)]
pub struct ParameterDto {
    pub key: String,
    pub label: String,
    pub description: String,
    pub unit: Option<String>,
    pub default: String,
    pub min: String,
    pub max: String,
    /// 该参数的展示类型（百分比参数要乘 100 显示）。
    pub display_as_percent: bool,
}

/// 数据覆盖情况。
#[derive(Debug, Serialize)]
pub struct CoverageDto {
    pub data_root: String,
    pub datasets: Vec<DatasetCoverageDto>,
    pub gaps: Vec<GapDto>,
}

#[derive(Debug, Serialize)]
pub struct DatasetCoverageDto {
    pub kind: String,
    pub symbol: String,
    pub partitions: usize,
    pub finalized: usize,
    pub first_month: Option<String>,
    pub last_month: Option<String>,
    pub parquet_bytes: u64,
    /// 异常分区（待查、失败）。界面必须显示，否则用户不知道数据有问题。
    pub problems: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GapDto {
    pub kind: String,
    pub symbol: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub note: String,
}

/// 下载请求。
#[derive(Debug, Deserialize)]
pub struct DownloadRequestDto {
    pub symbols: Vec<String>,
    /// 数据集名称：`klines` / `agg_trades` / `mark_price` / `funding`。
    pub kinds: Vec<String>,
    /// 起始月份 `YYYY-MM`。
    pub from: String,
    /// 结束月份 `YYYY-MM`。
    pub to: String,
}

/// 下载结果。
#[derive(Debug, Serialize)]
pub struct DownloadResultDto {
    pub planned: usize,
    pub completed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// 失败分区的说明。
    pub failures: Vec<String>,
}

/// 回测请求。
#[derive(Debug, Deserialize)]
pub struct BacktestRequestDto {
    pub symbol: String,
    pub strategy: Option<String>,
    /// `YYYY-MM-DD`。
    pub from: String,
    pub to: String,
    /// 成交模型列表，默认 `["m0","m1"]`。
    pub fill_models: Option<Vec<String>>,
    pub initial_equity: Option<String>,
}

/// 回测结果摘要。
#[derive(Debug, Serialize)]
pub struct BacktestResultDto {
    pub symbol: String,
    pub strategy_id: String,
    pub from: String,
    pub to: String,
    pub candle_count: usize,
    pub models: Vec<ModelResultDto>,
    /// 结论可信度裁决。**这是最重要的输出。**
    pub verdict: VerdictDto,
}

#[derive(Debug, Serialize)]
pub struct ModelResultDto {
    pub name: String,
    pub optimism: &'static str,
    pub final_equity: String,
    pub pnl: String,
    pub trade_count: usize,
    pub win_rate: Option<String>,
}

/// 结论可信度。前端必须显眼展示，不能藏在折叠面板里。
#[derive(Debug, Serialize)]
pub struct VerdictDto {
    /// 结论是否可信。
    pub conclusive: bool,
    /// 面向用户的一句话结论。
    pub message: String,
    /// 是否出现符号翻转（乐观与诚实模型方向相反）。
    pub sign_flips: bool,
    /// 盈亏平衡成交率。
    pub breakeven_fill_rate: Option<String>,
    /// 5 秒 markout 均值。负数说明存在系统性逆向选择。
    pub markout_5s: Option<String>,
    /// 费率来源非权威，结果不完整。
    pub fee_incomplete: bool,
    /// 止损触发未成交的次数与最大裸露时长。
    pub stop_exposure_events: usize,
    pub max_exposure_secs: i64,
    /// 0% 费率与常规费率下的盈亏对比。
    pub pnl_at_promotional_fee: String,
    pub pnl_at_standard_fee: String,
}

/// Markout 分析结果。
#[derive(Debug, Serialize)]
pub struct MarkoutDto {
    pub samples: usize,
    pub mean_1s: String,
    pub mean_5s: String,
    pub mean_30s: String,
    pub mean_5m: String,
    pub adverse_ratio_5s: String,
    /// 是否存在系统性逆向选择。
    pub adverse_selection: bool,
}

/// 成交模型信息。
#[derive(Debug, Serialize)]
pub struct FillModelDto {
    pub key: String,
    pub name: String,
    pub data_requirements: String,
    pub optimism: &'static str,
    pub optimism_note: String,
}

// ---------------------------------------------------------------------------
// 数值序列化
// ---------------------------------------------------------------------------

/// 把 `Decimal` 转成字符串，**去掉尾随零**。
///
/// `Decimal` 的 `to_string()` 会保留原始小数位，所以 `3200.00` 会输出成
/// `"3200.00"`，`25.0000` 输出成 `"25.0000"`。前端直接显示会很难看，而且
/// 同一数值在不同来源下字符串不同（`3200` vs `3200.00`）会让前端难以比较。
///
/// 传输的是**精确值**，只是表现形式规范化——不做舍入，不丢精度。
pub fn num(v: Decimal) -> String {
    let n = v.normalize();
    // normalize 会把 0 变成 "0"，负数零也规整掉
    let s = n.to_string();
    if s == "-0" { "0".to_string() } else { s }
}

// ---------------------------------------------------------------------------
// 领域类型 → DTO 的转换
// ---------------------------------------------------------------------------

pub fn mode_label(mode: ServiceMode) -> &'static str {
    mode.label()
}

pub fn mode_tag(mode: ServiceMode) -> &'static str {
    match mode {
        ServiceMode::Paper => "PAPER",
        ServiceMode::Live => "LIVE",
    }
}

pub fn side_tag(s: Side) -> &'static str {
    match s {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

pub fn side_label(s: Side) -> &'static str {
    match s {
        Side::Buy => "做多",
        Side::Sell => "做空",
    }
}

pub fn purpose_tag(p: domain::OrderPurpose) -> &'static str {
    match p {
        domain::OrderPurpose::Entry => "ENTRY",
        domain::OrderPurpose::TakeProfit => "TAKE_PROFIT",
        domain::OrderPurpose::StopLoss => "STOP_LOSS",
    }
}

pub fn purpose_label(p: domain::OrderPurpose) -> &'static str {
    match p {
        domain::OrderPurpose::Entry => "开仓",
        domain::OrderPurpose::TakeProfit => "止盈",
        domain::OrderPurpose::StopLoss => "止损",
    }
}

/// 把 `PositionView` 转成 DTO。
pub fn position_dto(v: &PositionView) -> PositionDto {
    PositionDto {
        symbol: v.symbol.clone(),
        side: side_tag(v.side),
        side_label: side_label(v.side),
        quantity: num(v.quantity),
        entry_price: num(v.entry_price),
        unrealized_pnl: num(v.unrealized_pnl),
        stop_price: v.stop_price.map(|p| num(p.get())),
        stop_triggered: v.stop_triggered,
        rungs: v
            .rungs
            .iter()
            .map(|r| RungDto {
                rung: r.rung,
                index: r.rung + 1,
                pct: num(r.pct),
                distance_bp: num(r.pct * Decimal::from(10_000)),
                fraction: num(r.fraction),
                price: num(r.price),
                filled: r.filled,
            })
            .collect(),
        realized_pnl: num(v.realized_pnl),
    }
}

/// 把 `ManualPreview` 转成 DTO。
pub fn preview_dto(p: &ManualPreview) -> ManualPreviewDto {
    ManualPreviewDto {
        entry: num(p.entry.get()),
        stop: num(p.stop.get()),
        quantity: num(p.quantity.get()),
        notional: num(p.notional),
        margin_required: num(p.margin_required),
        liquidation_buffer_pct: p.liquidation_buffer_pct.map(num),
        take_profits: p
            .take_profits
            .iter()
            .map(|r| RungPreviewDto {
                rung: r.rung,
                index: r.rung + 1,
                price: num(r.price.get()),
                quantity: num(r.quantity.get()),
                gross_profit: num(r.gross_profit),
                distance_bp: num(r.distance_bp),
            })
            .collect(),
        accepted: p.accepted,
        reject_reason: p.reject_reason.clone(),
        warnings: p.warnings.clone(),
    }
}

/// 把参数说明转成 DTO。
///
/// `display_as_percent` 的判定很关键：`%` 单位的参数在配置里存的是**比例**
/// （0.1 = 10%），前端若直接显示会小 100 倍。
pub fn parameter_dto(p: &domain::ParameterSpec) -> ParameterDto {
    ParameterDto {
        key: p.key.clone(),
        label: p.label.clone(),
        description: p.description.clone(),
        unit: p.unit.clone(),
        default: num(p.default),
        min: num(p.min),
        max: num(p.max),
        display_as_percent: p.unit.as_deref() == Some("%"),
    }
}

/// 把 `ManualPlanDto` 转成领域层的 `ManualPlan`。
///
/// 所有字符串都必须能解析为 `Decimal`——解析失败返回可读错误而不是默认值。
pub fn parse_manual_plan(dto: &ManualPlanDto, now: DateTime<Utc>) -> Result<ManualPlan, ApiError> {
    let dec = |s: &str, field: &str| -> Result<Decimal, ApiError> {
        s.trim()
            .parse::<Decimal>()
            .map_err(|_| ApiError::BadRequest(format!("字段 {field} 不是合法数值：{s}")))
    };

    let entry = dec(&dto.entry, "entry")?;
    let stop = dec(&dto.stop, "stop")?;
    let leverage = dec(&dto.leverage, "leverage")?;
    if leverage < Decimal::ONE {
        return Err(ApiError::BadRequest("杠杆必须不小于 1".into()));
    }

    let quantity = match &dto.quantity {
        Some(q) => Some(domain::Qty::new(dec(q, "quantity")?)),
        None => None,
    };
    let size_pct = match &dto.size_pct {
        Some(p) => Some(dec(p, "size_pct")?),
        None => None,
    };
    if quantity.is_none() && size_pct.is_none() {
        return Err(ApiError::BadRequest(
            "必须指定 quantity 或 size_pct 之一".into(),
        ));
    }

    let take_profit = match &dto.take_profit {
        Some(rungs) => {
            if rungs.is_empty() {
                return Err(ApiError::BadRequest("分批止盈不能为空".into()));
            }
            domain::TpPlan::Ladder {
                rungs: rungs
                    .iter()
                    .map(|r| {
                        Ok(domain::TpRung {
                            pct: dec(&r.pct, "take_profit.pct")?,
                            fraction: dec(&r.fraction, "take_profit.fraction")?,
                        })
                    })
                    .collect::<Result<Vec<_>, ApiError>>()?,
            }
        }
        None => {
            let pct = dto.take_profit_pct.as_deref().ok_or_else(|| {
                ApiError::BadRequest("必须指定 take_profit 或 take_profit_pct".into())
            })?;
            domain::TpPlan::Single {
                pct: dec(pct, "take_profit_pct")?,
            }
        }
    };
    take_profit
        .validate()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let break_even = match &dto.break_even {
        Some(b) => Some(domain::BreakEvenSpec {
            trigger_r: dec(&b.trigger_r, "break_even.trigger_r")?,
            offset: dec(&b.offset, "break_even.offset")?,
        }),
        None => None,
    };
    let trailing = match &dto.trailing {
        Some(t) => Some(domain::TrailingSpec {
            distance: dec(&t.distance, "trailing.distance")?,
            activate_at: match &t.activate_at {
                Some(a) => Some(dec(a, "trailing.activate_at")?),
                None => None,
            },
        }),
        None => None,
    };

    let cancel = dto
        .cancel_unfilled_after_secs
        .map(|s| now + chrono::Duration::seconds(s));

    Ok(ManualPlan {
        symbol: dto.symbol.clone(),
        side: dto.side,
        entry,
        quantity,
        size_pct,
        leverage,
        stop,
        take_profit,
        break_even,
        trailing,
        cancel_unfilled_after: cancel,
        client_ref: dto
            .client_ref
            .clone()
            .unwrap_or_else(|| "manual".to_string()),
    })
}

/// 让位原因的展示文案。
pub fn stand_down_message(reason: StandDownReason) -> String {
    reason.message().to_string()
}

/// 解析月份字符串。
pub fn parse_month(s: &str) -> Result<(i32, u32), ApiError> {
    let (y, m) = s
        .split_once('-')
        .ok_or_else(|| ApiError::BadRequest(format!("月份格式应为 YYYY-MM，收到：{s}")))?;
    let year: i32 = y
        .parse()
        .map_err(|_| ApiError::BadRequest(format!("非法年份：{y}")))?;
    let month: u32 = m
        .parse()
        .map_err(|_| ApiError::BadRequest(format!("非法月份：{m}")))?;
    if !(1..=12).contains(&month) {
        return Err(ApiError::BadRequest(format!(
            "月份必须在 1-12 之间：{month}"
        )));
    }
    Ok((year, month))
}

/// 解析日期。
pub fn parse_date(s: &str) -> Result<chrono::NaiveDate, ApiError> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| ApiError::BadRequest(format!("日期格式应为 YYYY-MM-DD，收到：{s}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{Price, Qty};
    use rust_decimal_macros::dec;

    fn base_dto() -> ManualPlanDto {
        ManualPlanDto {
            symbol: "ETHUSDC".into(),
            side: Side::Buy,
            entry: "3200".into(),
            quantity: Some("0.1".into()),
            size_pct: None,
            leverage: "3".into(),
            stop: "3192".into(),
            take_profit: None,
            take_profit_pct: Some("0.0025".into()),
            break_even: None,
            trailing: None,
            cancel_unfilled_after_secs: Some(120),
            client_ref: Some("manual".into()),
        }
    }

    /// `num()` 是所有数值传输的基础：去尾随零但不丢精度。
    ///
    /// 尾随零不去掉会让前端显示 `3200.00`、`25.0000` 这种难看的值，而且
    /// 同一数值在不同来源下字符串不同（`3200` vs `3200.00`）会妨碍比较。
    #[test]
    fn num_trims_trailing_zeros_without_losing_precision() {
        assert_eq!(num(dec!(3200.0000)), "3200");
        assert_eq!(num(dec!(25.0000)), "25");
        assert_eq!(num(dec!(0.00040)), "0.0004");
        assert_eq!(num(Decimal::ZERO), "0");
        // 高精度必须完整保留
        assert_eq!(num(dec!(3200.12345678)), "3200.12345678");
        // 负数与负零
        assert_eq!(num(dec!(-0.5)), "-0.5");
        assert_eq!(num(dec!(-0.0000000)), "0");
    }

    #[test]
    fn parses_valid_manual_plan() {
        let now = Utc::now();
        let p = parse_manual_plan(&base_dto(), now).unwrap();
        assert_eq!(p.entry, dec!(3200));
        assert_eq!(p.stop, dec!(3192));
        assert_eq!(p.quantity.unwrap().get(), dec!(0.1));
        assert_eq!(
            p.cancel_unfilled_after.unwrap(),
            now + chrono::Duration::seconds(120)
        );
    }

    /// 非法数值必须返回可读错误，不能用默认值兜底。
    #[test]
    fn invalid_decimal_is_rejected_with_field_name() {
        let mut dto = base_dto();
        dto.entry = "abc".into();
        let e = parse_manual_plan(&dto, Utc::now()).unwrap_err();
        assert!(e.to_string().contains("entry"), "{e}");
    }

    /// 数量和比例都没给必须报错。
    #[test]
    fn requires_quantity_or_size_pct() {
        let mut dto = base_dto();
        dto.quantity = None;
        let e = parse_manual_plan(&dto, Utc::now()).unwrap_err();
        assert!(e.to_string().contains("quantity"), "{e}");
    }

    /// 杠杆小于 1 没有意义。
    #[test]
    fn rejects_leverage_below_one() {
        let mut dto = base_dto();
        dto.leverage = "0.5".into();
        assert!(parse_manual_plan(&dto, Utc::now()).is_err());
    }

    /// 止盈计划必须二选一给出。
    #[test]
    fn requires_take_profit_specification() {
        let mut dto = base_dto();
        dto.take_profit_pct = None;
        let e = parse_manual_plan(&dto, Utc::now()).unwrap_err();
        assert!(e.to_string().contains("take_profit"), "{e}");
    }

    /// 分批止盈各档比例之和超过 1 必须被拒绝——否则会超卖。
    #[test]
    fn rejects_ladder_fractions_over_one() {
        let mut dto = base_dto();
        dto.take_profit_pct = None;
        dto.take_profit = Some(vec![
            TakeProfitRungDto {
                pct: "0.001".into(),
                fraction: "0.7".into(),
            },
            TakeProfitRungDto {
                pct: "0.002".into(),
                fraction: "0.7".into(),
            },
        ]);
        let e = parse_manual_plan(&dto, Utc::now()).unwrap_err();
        assert!(e.to_string().contains("超过"), "{e}");
    }

    #[test]
    fn rejects_empty_ladder() {
        let mut dto = base_dto();
        dto.take_profit_pct = None;
        dto.take_profit = Some(vec![]);
        assert!(parse_manual_plan(&dto, Utc::now()).is_err());
    }

    #[test]
    fn parses_ladder_and_break_even() {
        let mut dto = base_dto();
        dto.take_profit_pct = None;
        dto.take_profit = Some(vec![
            TakeProfitRungDto {
                pct: "0.001".into(),
                fraction: "0.5".into(),
            },
            TakeProfitRungDto {
                pct: "0.002".into(),
                fraction: "0.5".into(),
            },
        ]);
        dto.break_even = Some(BreakEvenDto {
            trigger_r: "1".into(),
            offset: "0".into(),
        });
        let p = parse_manual_plan(&dto, Utc::now()).unwrap();
        assert_eq!(p.take_profit.rungs().len(), 2);
        assert!(p.break_even.is_some());
    }

    #[test]
    fn default_client_ref_is_manual() {
        let mut dto = base_dto();
        dto.client_ref = None;
        assert_eq!(
            parse_manual_plan(&dto, Utc::now()).unwrap().client_ref,
            "manual"
        );
    }

    /// **`%` 单位的参数存的是比例，前端必须乘 100 显示。**
    #[test]
    fn percent_parameters_are_flagged_for_display() {
        let pct = domain::ParameterSpec {
            key: "equity_pct".into(),
            label: "仓位比例".into(),
            description: "d".into(),
            unit: Some("%".into()),
            default: dec!(0.1),
            min: dec!(0.001),
            max: Decimal::ONE,
        };
        let dto = parameter_dto(&pct);
        assert!(
            dto.display_as_percent,
            "百分比参数必须标记，否则前端会显示成 0.1% 而非 10%"
        );

        let bp = domain::ParameterSpec {
            key: "take_profit_bp".into(),
            label: "止盈".into(),
            description: "d".into(),
            unit: Some("基点".into()),
            default: dec!(4),
            min: Decimal::ONE,
            max: dec!(50),
        };
        assert!(!parameter_dto(&bp).display_as_percent);
    }

    /// 持仓 DTO 必须带止损触发标记——那是仓位裸露的警示。
    #[test]
    fn position_dto_surfaces_stop_trigger() {
        let v = PositionView {
            symbol: "ETHUSDC".into(),
            side: Side::Buy,
            quantity: dec!(0.6),
            entry_price: dec!(3200),
            unrealized_pnl: dec!(-5),
            stop_price: Some(Price::new(dec!(3192))),
            stop_triggered: true,
            rungs: vec![domain::RungView {
                rung: 0,
                pct: dec!(0.0025),
                fraction: dec!(0.5),
                price: dec!(3208),
                filled: true,
            }],
            realized_pnl: dec!(1.2),
        };
        let dto = position_dto(&v);
        assert!(dto.stop_triggered, "止损触发的裸露状态必须传到界面");
        assert_eq!(dto.rungs[0].index, 1, "档位序号从 1 开始给用户看");
        assert_eq!(dto.rungs[0].distance_bp, "25");
        assert_eq!(dto.side_label, "做多");
    }

    /// 预览 DTO 的价格必须是字符串——防止前端浮点丢精度。
    #[test]
    fn preview_prices_are_strings() {
        let p = ManualPreview {
            entry: Price::new(dec!(3200.12345678)),
            take_profits: vec![domain::RungPreview {
                rung: 0,
                price: Price::new(dec!(3208.01)),
                quantity: Qty::new(dec!(0.05)),
                gross_profit: dec!(0.39),
                distance_bp: dec!(25),
            }],
            stop: Price::new(dec!(3192)),
            quantity: Qty::new(dec!(0.1)),
            notional: dec!(320.01),
            margin_required: dec!(106.67),
            liquidation_buffer_pct: Some(dec!(0.01)),
            accepted: true,
            reject_reason: None,
            warnings: vec![],
        };
        let dto = preview_dto(&p);
        assert_eq!(dto.entry, "3200.12345678", "高精度价格必须无损传输");
        assert_eq!(dto.take_profits[0].index, 1);
    }

    /// 风控拒绝必须带原因——静默失败是恶劣的失败模式。
    #[test]
    fn rejected_preview_carries_reason() {
        let p = ManualPreview {
            entry: Price::new(dec!(3200)),
            take_profits: vec![],
            stop: Price::new(dec!(3210)),
            quantity: Qty::new(dec!(0.1)),
            notional: dec!(320),
            margin_required: dec!(106.67),
            liquidation_buffer_pct: None,
            accepted: false,
            reject_reason: Some("止损价晚于估算强平价".into()),
            warnings: vec!["止损距当前标记价不足 5%".into()],
        };
        let dto = preview_dto(&p);
        assert!(!dto.accepted);
        assert!(dto.reject_reason.is_some());
        assert!(!dto.warnings.is_empty(), "警告不能被吞掉");
    }

    #[test]
    fn month_parsing_validates() {
        assert_eq!(parse_month("2026-08").unwrap(), (2026, 8));
        assert!(parse_month("2026").is_err());
        assert!(parse_month("2026-13").is_err());
    }

    #[test]
    fn date_parsing_validates() {
        assert!(parse_date("2026-08-01").is_ok());
        assert!(parse_date("2026/08/01").is_err());
    }

    #[test]
    fn mode_labels_are_explicit() {
        assert_eq!(mode_tag(ServiceMode::Paper), "PAPER");
        assert_eq!(mode_tag(ServiceMode::Live), "LIVE");
        assert_eq!(mode_label(ServiceMode::Paper), "模拟盘");
        assert_eq!(mode_label(ServiceMode::Live), "实盘");
    }

    #[test]
    fn purpose_labels_are_chinese() {
        assert_eq!(purpose_label(domain::OrderPurpose::Entry), "开仓");
        assert_eq!(purpose_label(domain::OrderPurpose::TakeProfit), "止盈");
        assert_eq!(purpose_label(domain::OrderPurpose::StopLoss), "止损");
    }

    #[test]
    fn every_stand_down_reason_has_message() {
        for r in [
            StandDownReason::StaleFeed,
            StandDownReason::FeedDisconnected,
            StandDownReason::InsufficientHistory,
            StandDownReason::CandleNotClosed,
            StandDownReason::AlreadyEngaged,
            StandDownReason::SignalAlreadyUsed,
            StandDownReason::StopTooWide,
            StandDownReason::StopInsideLiquidation,
            StandDownReason::RiskRewardTooLow,
            StandDownReason::InsufficientEquity,
            StandDownReason::OutsideTradingHours,
        ] {
            assert!(!stand_down_message(r).is_empty());
        }
    }
}
