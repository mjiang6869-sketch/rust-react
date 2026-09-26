//! `/api/v1/auto-maker`：区间做市策略的自动化开关与参数配置。
//!
//! # 只支持模拟盘，且只支持一个策略
//!
//! 目前只包了 `range_maker` 一个策略，且只允许在模拟盘（`ServiceMode::Paper`）
//! 下启用——实盘的自动化做市涉及真实资金，需要单独的上线流程与更严格的
//! 审阅，不是这一步的范围。启用（`enabled: true`）时若当前处于实盘模式，
//! 返回 409。关闭永远允许，不做模式检查。
//!
//! **默认关闭，重启也关闭**：`engine::PaperEngine` 里 `auto_maker_enabled`
//! 不落库（见该字段文档），进程重启后回到关闭状态，必须由操作者重新打开。
//! 切到实盘也会自动关闭（见 `state::AppState::set_mode`）——这里的模式检查
//! 只是防住「先切实盘再打开」这条路径，不是唯一的防线。
//!
//! # 参数白名单
//!
//! 可编辑字段只有 [`crate::dto::AutoMakerParamsDto`] 里列出的那些；
//! `break_even`、`trailing`、`trailing_bp`、`one_signal_per_bar` 为什么不
//! 开放，见该类型的文档。PUT 时这四个字段永远保持引擎当前生效值不变——
//! 这里的 [`merge_params`] 只覆盖白名单字段，其余字段原样从当前参数拷贝。

use std::sync::Arc;

use axum::{Json, extract::State, response::IntoResponse};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use strategies::RangeMakerParams;

use crate::dto::{
    ApiError, ApiResponse, AutoMakerDto, AutoMakerParamsDto, Json2, ParameterDto, auto_maker_dto,
    parameter_dto,
};
use crate::state::AppState;

/// GET/PUT 共用的响应体：运行状态视图 + 可编辑字段的说明。
#[derive(Debug, Serialize)]
pub struct AutoMakerConfigDto {
    #[serde(flatten)]
    pub state: AutoMakerDto,
    /// 可编辑字段的说明（标签、单位、取值范围），供前端渲染表单。
    pub fields: Vec<ParameterDto>,
}

/// 可编辑参数白名单，必须与 [`AutoMakerParamsDto`] 的字段一一对应。
const EDITABLE_KEYS: &[&str] = &[
    "lookback",
    "take_profit_bp",
    "stop_buffer_bp",
    "equity_pct",
    "leverage",
    "valid_minutes",
];

fn editable_fields() -> Vec<ParameterDto> {
    RangeMakerParams::specs()
        .iter()
        .filter(|s| EDITABLE_KEYS.contains(&s.key.as_str()))
        .map(parameter_dto)
        .collect()
}

pub async fn get(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let engine = s.engine.lock().await;
    let view = engine.auto_maker();
    Json(ApiResponse::ok(AutoMakerConfigDto {
        state: auto_maker_dto(&view),
        fields: editable_fields(),
    }))
}

/// PUT 请求体。`deny_unknown_fields`：拼错的字段名应该报错，而不是被
/// 静默忽略后让用户以为设置生效了。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutAutoMakerDto {
    pub enabled: bool,
    /// 省略时保持当前生效参数不变，只切换开关。
    pub params: Option<AutoMakerParamsDto>,
}

fn parse_usize(s: &str, field: &str) -> Result<usize, ApiError> {
    s.trim()
        .parse::<usize>()
        .map_err(|_| ApiError::BadRequest(format!("字段 {field} 须为正整数：{s}")))
}

fn parse_i64(s: &str, field: &str) -> Result<i64, ApiError> {
    s.trim()
        .parse::<i64>()
        .map_err(|_| ApiError::BadRequest(format!("字段 {field} 须为正整数：{s}")))
}

fn parse_decimal(s: &str, field: &str) -> Result<Decimal, ApiError> {
    s.trim()
        .parse::<Decimal>()
        .map_err(|_| ApiError::BadRequest(format!("字段 {field} 须为数字：{s}")))
}

/// 把请求里的白名单字段覆盖到 `base`（引擎当前生效参数）上。
///
/// 这里只做**格式**解析（字符串能不能转成对应类型），范围校验交给
/// `RangeMakerParams::validate`（在 `engine::configure_auto_maker` 内部
/// 调用）——避免这里的范围判断与引擎的判断分裂。
fn merge_params(
    base: &RangeMakerParams,
    dto: &AutoMakerParamsDto,
) -> Result<RangeMakerParams, ApiError> {
    let mut merged = base.clone();
    merged.lookback = parse_usize(&dto.lookback, "lookback")?;
    merged.take_profit_bp = parse_decimal(&dto.take_profit_bp, "take_profit_bp")?;
    merged.stop_buffer_bp = parse_decimal(&dto.stop_buffer_bp, "stop_buffer_bp")?;
    merged.side_mode = dto.side_mode;
    merged.equity_pct = parse_decimal(&dto.equity_pct, "equity_pct")?;
    merged.leverage = parse_decimal(&dto.leverage, "leverage")?;
    merged.valid_minutes = parse_i64(&dto.valid_minutes, "valid_minutes")?;
    Ok(merged)
}

pub async fn put(
    State(s): State<Arc<AppState>>,
    Json2(req): Json2<PutAutoMakerDto>,
) -> Result<impl IntoResponse, ApiError> {
    let mut engine = s.engine.lock().await;

    let params = match &req.params {
        Some(dto) => Some(merge_params(&engine.auto_maker().params, dto)?),
        None => None,
    };

    // 实盘保护：只在尝试启用时检查，且必须在**持有引擎锁期间**读取模式。
    //
    // 锁顺序：这里是"先拿引擎锁，再短暂拿一次 mode 锁"（`s.mode()` 内部
    // 拿到值后立即释放）。`state::AppState::set_mode` 里是反过来的独立两步
    // ——先完全释放 mode 锁，再单独拿引擎锁——两个方向不会同时嵌套等待
    // 对方持有的锁，所以不会死锁。即便请求交错，"切到实盘"最终一定会把
    // 已启用的自动化做市关掉（`set_mode` 负责），这里只是防住
    // "先切实盘、再尝试打开"这条路径。
    if req.enabled {
        let mode = s.mode().await;
        if mode.is_live() {
            return Err(ApiError::Conflict(
                "实盘模式下不能启用自动化做市：本次只支持模拟盘".into(),
            ));
        }
    }

    engine
        .configure_auto_maker(req.enabled, params)
        .map_err(ApiError::BadRequest)?;

    let view = engine.auto_maker();
    Ok(Json(ApiResponse::ok(AutoMakerConfigDto {
        state: auto_maker_dto(&view),
        fields: editable_fields(),
    })))
}
