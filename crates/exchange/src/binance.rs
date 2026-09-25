//! 币安 USDⓈ-M 合约规则解析与执行客户端。
//!
//! # 这个文件处理的两个币安特性，直接影响能不能做市
//!
//! ## 1. `triggerProtect`：条件单触发价必须距标记价至少 5%
//!
//! 币安对条件单（STOP / TAKE_PROFIT 系列）有防误触保护：触发价距离标记价
//! 不足 `triggerProtect`（实测 0.05 = 5%）时**直接拒单**。
//!
//! 这对做市是硬限制——做市的止损往往就在市价附近几个基点。所以：
//! - **入场单用限价（GTX）**，不受此约束
//! - **止损**在近价位时不能依赖条件单，必须用**限价单**挂出
//! - 只有远离市价的止损才适合用 `STOP` 条件单
//!
//! `check_trigger_protect` 把这个判断显式化，让调用方能选择退化路径。
//!
//! ## 2. 没有 OCO / bracket
//!
//! 分批止盈与止损必须由 `domain::position_set` 自己管理（见该模块文档）。

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use domain::{ContractKind, FeeSchedule, FeeSource, Instrument, Precision, RejectReason};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::ExchangeError;

/// 从 `exchangeInfo` 解析出的合约规则。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractSpec {
    pub symbol: String,
    pub contract_type: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub margin_asset: String,
    pub status: String,
    pub precision: Precision,
    /// 维持保证金率（`maintMarginPercent`，百分比）。
    ///
    /// **必须从这里读取，不能硬编码。** 旧实现写死 0.4%，真实值是 2.5%，
    /// 导致高杠杆下误判止损不安全并静默拒绝信号。
    pub maint_margin_pct: Decimal,
    pub required_margin_pct: Decimal,
    pub liquidation_fee: Decimal,
    /// 条件单触发价距标记价的最小距离比例（`triggerProtect`）。
    pub trigger_protect: Option<Decimal>,
    /// 该合约支持的条件单类型。
    pub order_types: Vec<String>,
    /// 上线时间（毫秒）。
    pub onboard_ms: Option<i64>,
}

impl ContractSpec {
    /// 是否支持 post-only（GTX）。做市的前提。
    pub fn supports_post_only(&self) -> bool {
        self.order_types.iter().any(|t| t == "LIMIT")
    }

    /// 该合约是否可交易。
    pub fn is_trading(&self) -> bool {
        self.status == "TRADING"
    }

    /// 转成领域层的 `Instrument`。
    pub fn to_instrument(&self, fees: FeeSchedule) -> Instrument {
        let kind = if self.contract_type == "TRADIFI_PERPETUAL" {
            ContractKind::TradFiPerp
        } else {
            ContractKind::CryptoPerp
        };
        Instrument {
            symbol: self.symbol.clone(),
            kind,
            base_asset: self.base_asset.clone(),
            quote_asset: self.quote_asset.clone(),
            margin_asset: self.margin_asset.clone(),
            // 结算资产：币安以 margin_asset 结算盈亏。
            settlement_asset: self.margin_asset.clone(),
            precision: self.precision.clone(),
            maint_margin_pct: self.maint_margin_pct,
            required_margin_pct: self.required_margin_pct,
            liquidation_fee: self.liquidation_fee,
            fees,
        }
    }
}

/// 条件单触发价是否满足币安的防误触保护。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerProtectVerdict {
    /// 满足要求，可以用条件单。
    Ok,
    /// 触发价离标记价太近，币安会拒单。
    ///
    /// 调用方应改用**限价单**挂出（做市的常见选择），或把触发价移远。
    TooClose {
        /// 当前距离比例。
        distance: Decimal,
        /// 币安要求的最小距离。
        required: Decimal,
    },
}

impl TriggerProtectVerdict {
    pub fn is_ok(&self) -> bool {
        matches!(self, TriggerProtectVerdict::Ok)
    }
}

/// 检查条件单触发价是否满足 `triggerProtect`。
///
/// `mark_price` 是标记价。币安用标记价（而非最新成交价）判断这个距离。
pub fn check_trigger_protect(
    spec: &ContractSpec,
    trigger_price: Decimal,
    mark_price: Decimal,
) -> TriggerProtectVerdict {
    let Some(required) = spec.trigger_protect else {
        // 该合约没有这个字段，视为无约束。
        return TriggerProtectVerdict::Ok;
    };
    if mark_price <= Decimal::ZERO {
        return TriggerProtectVerdict::Ok;
    }
    let distance = ((trigger_price - mark_price) / mark_price).abs();
    if distance >= required {
        TriggerProtectVerdict::Ok
    } else {
        TriggerProtectVerdict::TooClose { distance, required }
    }
}

/// `exchangeInfo` 的响应结构（只取需要的字段）。
#[derive(Debug, Deserialize)]
pub struct ExchangeInfoResponse {
    #[serde(default)]
    pub symbols: Vec<RawSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawSymbol {
    pub symbol: String,
    #[serde(default)]
    pub contract_type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub base_asset: String,
    #[serde(default)]
    pub quote_asset: String,
    #[serde(default)]
    pub margin_asset: String,
    #[serde(default)]
    pub maint_margin_percent: String,
    #[serde(default)]
    pub required_margin_percent: String,
    #[serde(default)]
    pub liquidation_fee: String,
    #[serde(default)]
    pub trigger_protect: Option<String>,
    #[serde(default)]
    pub order_types: Vec<String>,
    #[serde(default)]
    pub onboard_date: Option<i64>,
    #[serde(default)]
    pub filters: Vec<RawFilter>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawFilter {
    pub filter_type: String,
    #[serde(default)]
    pub tick_size: Option<String>,
    #[serde(default)]
    pub step_size: Option<String>,
    #[serde(default)]
    pub min_qty: Option<String>,
    #[serde(default)]
    pub notional: Option<String>,
}

/// 解析单个合约规则。
pub fn parse_contract(raw: &RawSymbol) -> Result<ContractSpec, ExchangeError> {
    let mut tick = None;
    let mut step = None;
    let mut min_qty = None;
    let mut notional = None;

    for f in &raw.filters {
        match f.filter_type.as_str() {
            "PRICE_FILTER" => tick = f.tick_size.clone(),
            "LOT_SIZE" => {
                step = f.step_size.clone();
                min_qty = f.min_qty.clone();
            }
            "MIN_NOTIONAL" => notional = f.notional.clone(),
            _ => {}
        }
    }

    let parse_dec = |s: Option<&str>, field: &str| -> Result<Decimal, ExchangeError> {
        let v = s.ok_or_else(|| ExchangeError::Fatal(format!("{} 缺少 {field}", raw.symbol)))?;
        v.parse::<Decimal>()
            .map_err(|_| ExchangeError::Fatal(format!("{} 的 {field} 无法解析：{v}", raw.symbol)))
    };

    let parse_pct = |s: &str| -> Decimal {
        // 空字符串按 0 处理（部分合约不返回该字段）
        s.parse::<Decimal>().unwrap_or(Decimal::ZERO)
    };

    let precision = Precision {
        tick_size: parse_dec(tick.as_deref(), "tickSize")?,
        step_size: parse_dec(step.as_deref(), "stepSize")?,
        min_qty: parse_dec(min_qty.as_deref(), "minQty")?,
        // 缺失时用 5 作为保守默认——这是币安多数合约的实际值。
        min_notional: notional
            .as_deref()
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::from(5)),
    };

    Ok(ContractSpec {
        symbol: raw.symbol.clone(),
        contract_type: raw.contract_type.clone(),
        base_asset: raw.base_asset.clone(),
        quote_asset: raw.quote_asset.clone(),
        margin_asset: raw.margin_asset.clone(),
        status: raw.status.clone(),
        precision,
        maint_margin_pct: parse_pct(&raw.maint_margin_percent),
        required_margin_pct: parse_pct(&raw.required_margin_percent),
        liquidation_fee: parse_pct(&raw.liquidation_fee),
        trigger_protect: raw
            .trigger_protect
            .as_deref()
            .and_then(|s| s.parse::<Decimal>().ok()),
        order_types: raw.order_types.clone(),
        onboard_ms: raw.onboard_date,
    })
}

/// 从 `exchangeInfo` 响应里找出指定合约。
pub fn find_contract(
    resp: &ExchangeInfoResponse,
    symbol: &str,
) -> Result<ContractSpec, ExchangeError> {
    let raw = resp
        .symbols
        .iter()
        .find(|s| s.symbol == symbol)
        .ok_or_else(|| ExchangeError::Fatal(format!("exchangeInfo 里没有合约 {symbol}")))?;
    parse_contract(raw)
}

/// 解析全部合约，按 symbol 索引。
pub fn parse_all_contracts(
    resp: &ExchangeInfoResponse,
) -> (BTreeMap<String, ContractSpec>, Vec<String>) {
    let mut map = BTreeMap::new();
    let mut errors = Vec::new();
    for raw in &resp.symbols {
        match parse_contract(raw) {
            Ok(spec) => {
                map.insert(spec.symbol.clone(), spec);
            }
            Err(e) => errors.push(format!("{}: {e}", raw.symbol)),
        }
    }
    (map, errors)
}

/// 币安账户费率响应（`/fapi/v2/account` 的相关字段）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountFeeResponse {
    #[serde(default)]
    pub maker_commission: String,
    #[serde(default)]
    pub taker_commission: String,
}

/// 从账户响应构造费率快照。
///
/// **这是唯一能让费率为"权威"的来源。** 零费率活动是策略 edge 的全部来源，
/// 所以必须与账户对账，不能靠假设。
pub fn fee_schedule_from_account(resp: &AccountFeeResponse, at: DateTime<Utc>) -> FeeSchedule {
    // 币安返回的是百分比数值（例如 "0.0002" 表示 0.02%），需要除以 100
    // 转成小数比例。
    let to_ratio = |s: &str| -> Decimal {
        s.parse::<Decimal>()
            .map(|v| v / Decimal::from(100))
            .unwrap_or(Decimal::ZERO)
    };
    FeeSchedule {
        maker_rate: to_ratio(&resp.maker_commission),
        taker_rate: to_ratio(&resp.taker_commission),
        source: FeeSource::ExchangeAccount,
        observed_at: at,
    }
}

/// 币安错误码 → 拒单原因。
///
/// 错误码 5022 是 post-only 会立即成交——**做市最常见的"错误"**，且这类
/// 订单不记入历史、不推送事件，所以必须在此正确识别而不是当成异常。
pub fn reject_reason_from_code(code: i64, msg: &str) -> Option<RejectReason> {
    match code {
        -5022 => Some(RejectReason::PostOnlyWouldCross),
        -2019 => Some(RejectReason::InsufficientMargin),
        -4131 | -4003 | -4004 | -4016 => Some(RejectReason::InvalidQuantity),
        -4140 => Some(RejectReason::InstrumentNotTrading),
        -2021 => Some(RejectReason::PriceOutOfRange), // 会立即触发
        _ => {
            if msg.contains("reduceOnly") {
                Some(RejectReason::InvalidQuantity)
            } else {
                None
            }
        }
    }
}

/// 币安 API 错误响应。
#[derive(Debug, Deserialize)]
pub struct BinanceError {
    pub code: i64,
    pub msg: String,
}

/// 把币安的 HTTP 响应转成分类后的错误。
///
/// 分类依据是**能否安全重试**，而不是错误文本。
pub fn classify_api_error(status: u16, body: &str) -> ExchangeError {
    // 限流：明确的、可等待后重试的
    if status == 429 || status == 418 {
        return ExchangeError::RateLimited {
            retry_after_ms: 1000,
        };
    }

    // 5xx：状态未知，必须先查询对账
    if status >= 500 {
        return ExchangeError::Unknown(format!("HTTP {status}：{body}"));
    }

    // 4xx：尝试解析币安错误码
    if let Ok(err) = serde_json::from_str::<BinanceError>(body) {
        if let Some(reason) = reject_reason_from_code(err.code, &err.msg) {
            return ExchangeError::Rejected(reason);
        }
        // 401/403 类属于凭据问题，是致命错误
        if status == 401 || status == 403 {
            return ExchangeError::Fatal(format!("凭据或权限问题：{}", err.msg));
        }
        return ExchangeError::Definitive(format!("币安拒绝（{}）：{}", err.code, err.msg));
    }

    ExchangeError::Definitive(format!("HTTP {status}：{body}"))
}

/// 下单请求。
#[derive(Clone, Debug, Serialize)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub quantity: String,
    #[serde(rename = "newClientOrderId")]
    pub client_order_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(rename = "timeInForce", skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<String>,
    #[serde(rename = "reduceOnly", skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
    #[serde(rename = "stopPrice", skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
    #[serde(rename = "workingType", skip_serializing_if = "Option::is_none")]
    pub working_type: Option<String>,
    #[serde(rename = "priceProtect", skip_serializing_if = "Option::is_none")]
    pub price_protect: Option<bool>,
    #[serde(rename = "goodTillDate", skip_serializing_if = "Option::is_none")]
    pub good_till_date: Option<i64>,
}

impl OrderRequest {
    /// 构造一张 post-only 限价单。
    ///
    /// 这是做市唯一允许的入场单类型。时间和时效参数用 `GTX`（只做 maker）。
    pub fn post_only_limit(
        symbol: &str,
        side: domain::Side,
        quantity: Decimal,
        price: Decimal,
        client_order_id: &str,
    ) -> Self {
        Self {
            symbol: symbol.to_string(),
            side: side_tag(side).to_string(),
            order_type: "LIMIT".to_string(),
            quantity: quantity.to_string(),
            client_order_id: client_order_id.to_string(),
            price: Some(price.to_string()),
            time_in_force: Some("GTX".to_string()),
            reduce_only: None,
            stop_price: None,
            working_type: None,
            price_protect: None,
            good_till_date: None,
        }
    }

    /// 构造一张 reduce-only 的 post-only 出场单。
    ///
    /// 止盈与止损都用这个——**maker-only 下止损也是挂单**，代价是可能不成交，
    /// 但好处是零手续费。
    pub fn reduce_only_limit(
        symbol: &str,
        side: domain::Side,
        quantity: Decimal,
        price: Decimal,
        client_order_id: &str,
    ) -> Self {
        Self {
            reduce_only: Some(true),
            ..Self::post_only_limit(symbol, side, quantity, price, client_order_id)
        }
    }

    /// 构造一张条件单（STOP / TAKE_PROFIT）。
    ///
    /// **注意 `triggerProtect` 约束**：触发价距标记价不足 `triggerProtect`
    /// 时币安会拒单。做市场景下优先用 `reduce_only_limit` 而非条件单。
    ///
    /// `working_type` 建议用 `MARK_PRICE`：用最新成交价容易被插针扫掉。
    pub fn conditional(
        symbol: &str,
        side: domain::Side,
        quantity: Decimal,
        trigger_price: Decimal,
        limit_price: Decimal,
        client_order_id: &str,
        is_stop: bool,
    ) -> Self {
        Self {
            symbol: symbol.to_string(),
            side: side_tag(side).to_string(),
            order_type: if is_stop { "STOP" } else { "TAKE_PROFIT" }.to_string(),
            quantity: quantity.to_string(),
            client_order_id: client_order_id.to_string(),
            price: Some(limit_price.to_string()),
            time_in_force: Some("GTC".to_string()),
            reduce_only: Some(true),
            stop_price: Some(trigger_price.to_string()),
            working_type: Some("MARK_PRICE".to_string()),
            price_protect: Some(true),
            good_till_date: None,
        }
    }
}

pub fn side_tag(s: domain::Side) -> &'static str {
    match s {
        domain::Side::Buy => "BUY",
        domain::Side::Sell => "SELL",
    }
}

/// 币安下单响应（`newOrderRespType=RESULT`）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderResponse {
    #[serde(default)]
    pub order_id: i64,
    #[serde(default)]
    pub client_order_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub avg_price: String,
    #[serde(default)]
    pub executed_qty: String,
    #[serde(default)]
    pub price: String,
}

/// 已接受的订单。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedOrder {
    pub exchange_order_id: String,
    pub client_order_id: String,
    pub status: String,
    pub executed_qty: Decimal,
    pub avg_price: Decimal,
}

impl TryFrom<OrderResponse> for AcceptedOrder {
    type Error = ExchangeError;

    fn try_from(r: OrderResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            exchange_order_id: r.order_id.to_string(),
            client_order_id: r.client_order_id,
            status: r.status,
            executed_qty: r.executed_qty.parse().unwrap_or(Decimal::ZERO),
            avg_price: r.avg_price.parse().unwrap_or(Decimal::ZERO),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn spec() -> ContractSpec {
        ContractSpec {
            symbol: "ETHUSDC".into(),
            contract_type: "PERPETUAL".into(),
            base_asset: "ETH".into(),
            quote_asset: "USDC".into(),
            margin_asset: "USDC".into(),
            status: "TRADING".into(),
            precision: Precision {
                tick_size: dec!(0.01),
                step_size: dec!(0.001),
                min_qty: dec!(0.001),
                min_notional: dec!(5),
            },
            maint_margin_pct: dec!(2.5),
            required_margin_pct: dec!(5),
            liquidation_fee: dec!(0.0125),
            trigger_protect: Some(dec!(0.05)),
            order_types: vec!["LIMIT".into(), "MARKET".into(), "STOP".into()],
            onboard_ms: None,
        }
    }

    fn raw_symbol() -> RawSymbol {
        serde_json::from_str(
            r#"{
                "symbol": "ETHUSDC",
                "contractType": "PERPETUAL",
                "status": "TRADING",
                "baseAsset": "ETH",
                "quoteAsset": "USDC",
                "marginAsset": "USDC",
                "maintMarginPercent": "2.5000",
                "requiredMarginPercent": "5.0000",
                "liquidationFee": "0.012500",
                "triggerProtect": "0.0500",
                "orderTypes": ["LIMIT","MARKET","STOP","GTX"],
                "onboardDate": 1775569200000,
                "filters": [
                    {"filterType":"PRICE_FILTER","tickSize":"0.01"},
                    {"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001"},
                    {"filterType":"MIN_NOTIONAL","notional":"5"}
                ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_contract_rules_from_exchange_info() {
        let c = parse_contract(&raw_symbol()).unwrap();
        assert_eq!(c.symbol, "ETHUSDC");
        assert_eq!(c.precision.tick_size, dec!(0.01));
        assert_eq!(c.precision.step_size, dec!(0.001));
        assert_eq!(c.precision.min_notional, dec!(5));
        assert_eq!(c.trigger_protect, Some(dec!(0.05)));
        assert_eq!(c.onboard_ms, Some(1775569200000));
    }

    /// 维持保证金率必须从交易所读取——旧实现硬编码 0.4% 而真实值是 2.5%。
    #[test]
    fn maintenance_margin_is_read_from_exchange_not_hardcoded() {
        let c = parse_contract(&raw_symbol()).unwrap();
        assert_eq!(
            c.maint_margin_pct,
            dec!(2.5),
            "必须用交易所返回的 2.5%，不是硬编码的 0.4%"
        );
        assert_eq!(c.required_margin_pct, dec!(5));
        assert_eq!(c.liquidation_fee, dec!(0.0125));
    }

    /// **triggerProtect 是做市的硬限制。**
    ///
    /// 触发价距标记价不足 5% 会被币安拒单，而做市的止损往往就在市价附近。
    #[test]
    fn trigger_protect_rejects_close_triggers() {
        let s = spec();
        // 标记价 3200，触发价 3199 -> 距离 0.03% < 5%
        let v = check_trigger_protect(&s, dec!(3199), dec!(3200));
        assert!(!v.is_ok(), "近价位触发必须被识别");
        match v {
            TriggerProtectVerdict::TooClose { distance, required } => {
                assert_eq!(required, dec!(0.05));
                assert!(distance < required);
            }
            _ => panic!("应为 TooClose"),
        }
    }

    #[test]
    fn trigger_protect_allows_far_triggers() {
        let s = spec();
        // 标记价 3200，触发价 3000 -> 距离 6.25% > 5%
        assert!(check_trigger_protect(&s, dec!(3000), dec!(3200)).is_ok());
        assert!(check_trigger_protect(&s, dec!(3400), dec!(3200)).is_ok());
    }

    /// 没有 triggerProtect 字段的合约不受约束。
    #[test]
    fn absent_trigger_protect_imposes_no_limit() {
        let mut s = spec();
        s.trigger_protect = None;
        assert!(check_trigger_protect(&s, dec!(3199), dec!(3200)).is_ok());
    }

    #[test]
    fn zero_mark_price_does_not_panic() {
        let s = spec();
        assert!(check_trigger_protect(&s, dec!(100), Decimal::ZERO).is_ok());
    }

    #[test]
    fn contract_converts_to_instrument() {
        let c = parse_contract(&raw_symbol()).unwrap();
        let fees = FeeSchedule {
            maker_rate: Decimal::ZERO,
            taker_rate: dec!(0.0005),
            source: FeeSource::ExchangeAccount,
            observed_at: Utc::now(),
        };
        let i = c.to_instrument(fees);
        assert_eq!(i.symbol, "ETHUSDC");
        assert_eq!(i.kind, ContractKind::CryptoPerp);
        assert_eq!(i.settlement_asset, "USDC");
        assert_eq!(i.maint_margin_pct, dec!(2.5));
    }

    #[test]
    fn tradfi_contract_type_is_recognized() {
        let mut raw = raw_symbol();
        raw.contract_type = "TRADIFI_PERPETUAL".into();
        let c = parse_contract(&raw).unwrap();
        let fees = FeeSchedule {
            maker_rate: Decimal::ZERO,
            taker_rate: dec!(0.0005),
            source: FeeSource::PromotionalAssumed,
            observed_at: Utc::now(),
        };
        assert_eq!(c.to_instrument(fees).kind, ContractKind::TradFiPerp);
    }

    /// 缺少必需的过滤器时必须是明确错误，不能静默用默认值。
    #[test]
    fn missing_required_filter_is_an_error() {
        let mut raw = raw_symbol();
        raw.filters.retain(|f| f.filter_type != "PRICE_FILTER");
        let err = parse_contract(&raw).unwrap_err().to_string();
        assert!(err.contains("tickSize"), "{err}");
    }

    /// **零费率活动是策略 edge 的全部来源，费率必须从账户对账。**
    #[test]
    fn fee_schedule_from_account_is_authoritative() {
        let resp = AccountFeeResponse {
            maker_commission: "0".into(),
            taker_commission: "0.05".into(),
        };
        let f = fee_schedule_from_account(&resp, Utc::now());
        assert_eq!(f.maker_rate, Decimal::ZERO, "零费率活动下 maker 应为 0");
        assert_eq!(f.taker_rate, dec!(0.0005), "0.05% 转成比例是 0.0005");
        assert_eq!(f.source, FeeSource::ExchangeAccount);
        assert!(f.source.is_authoritative(), "账户来源是权威费率");
    }

    /// 错误码 5022 是做市最常见的"错误"：post-only 会立即成交。
    /// 这类订单不记入历史、不推送事件，必须被正确识别。
    #[test]
    fn post_only_rejection_is_recognized() {
        let r = reject_reason_from_code(-5022, "Order would immediately match and take.");
        assert_eq!(r, Some(RejectReason::PostOnlyWouldCross));
    }

    #[test]
    fn other_rejection_codes_are_mapped() {
        assert_eq!(
            reject_reason_from_code(-2019, "Margin is insufficient."),
            Some(RejectReason::InsufficientMargin)
        );
        assert_eq!(
            reject_reason_from_code(-4140, "Symbol not trading."),
            Some(RejectReason::InstrumentNotTrading)
        );
        assert_eq!(reject_reason_from_code(-9999, "unknown"), None);
    }

    /// 5xx 必须归为 Unknown——状态未知，需要查询对账，**绝不能直接重试**。
    #[test]
    fn server_errors_are_classified_as_unknown() {
        let e = classify_api_error(500, "internal error");
        assert!(matches!(e, ExchangeError::Unknown(_)));
        assert!(
            !e.retryable_without_reconcile(),
            "状态未知时不能直接重发订单"
        );
    }

    #[test]
    fn rate_limit_is_classified_separately() {
        assert!(matches!(
            classify_api_error(429, "too many requests"),
            ExchangeError::RateLimited { .. }
        ));
        assert!(matches!(
            classify_api_error(418, "banned"),
            ExchangeError::RateLimited { .. }
        ));
    }

    #[test]
    fn post_only_rejection_surfaces_as_rejected() {
        let body = r#"{"code":-5022,"msg":"Order would immediately match and take."}"#;
        let e = classify_api_error(400, body);
        assert!(matches!(
            e,
            ExchangeError::Rejected(RejectReason::PostOnlyWouldCross)
        ));
        assert!(!e.retryable_without_reconcile(), "被拒的单不该重试");
    }

    #[test]
    fn auth_errors_are_fatal() {
        let body = r#"{"code":-2015,"msg":"Invalid API-key, IP, or permissions"}"#;
        let e = classify_api_error(401, body);
        assert!(e.is_fatal(), "凭据问题应停止交易：{e:?}");
    }

    /// post-only 限价单是唯一允许的入场方式。
    #[test]
    fn post_only_limit_order_uses_gtx() {
        let r = OrderRequest::post_only_limit(
            "ETHUSDC",
            domain::Side::Buy,
            dec!(0.1),
            dec!(3200),
            "mm:1",
        );
        assert_eq!(r.order_type, "LIMIT");
        assert_eq!(r.time_in_force.as_deref(), Some("GTX"));
        assert_eq!(r.reduce_only, None, "开仓单不带 reduceOnly");
        assert_eq!(r.side, "BUY");
    }

    /// 出场单必须是 reduce-only，否则可能反向开仓。
    #[test]
    fn exit_orders_are_reduce_only() {
        let r = OrderRequest::reduce_only_limit(
            "ETHUSDC",
            domain::Side::Sell,
            dec!(0.1),
            dec!(3210),
            "mm:1:tp",
        );
        assert_eq!(r.reduce_only, Some(true));
        assert_eq!(r.time_in_force.as_deref(), Some("GTX"), "出场也用 maker");
    }

    /// 条件单要用标记价触发 + 开启防误触保护。
    #[test]
    fn conditional_orders_use_mark_price_and_protection() {
        let r = OrderRequest::conditional(
            "ETHUSDC",
            domain::Side::Sell,
            dec!(0.1),
            dec!(3000),
            dec!(2999),
            "mm:1:stop",
            true,
        );
        assert_eq!(r.order_type, "STOP");
        assert_eq!(r.working_type.as_deref(), Some("MARK_PRICE"));
        assert_eq!(r.price_protect, Some(true));
        assert_eq!(r.reduce_only, Some(true));
        assert_eq!(r.stop_price.as_deref(), Some("3000"));
    }

    #[test]
    fn take_profit_conditional_uses_correct_type() {
        let r = OrderRequest::conditional(
            "ETHUSDC",
            domain::Side::Sell,
            dec!(0.1),
            dec!(3400),
            dec!(3401),
            "mm:1:tp",
            false,
        );
        assert_eq!(r.order_type, "TAKE_PROFIT");
    }

    #[test]
    fn order_response_is_parsed() {
        let resp: OrderResponse = serde_json::from_str(
            r#"{"orderId":123456,"clientOrderId":"mm:1","status":"NEW","avgPrice":"0","executedQty":"0","price":"3200"}"#,
        )
        .unwrap();
        let a = AcceptedOrder::try_from(resp).unwrap();
        assert_eq!(a.exchange_order_id, "123456");
        assert_eq!(a.client_order_id, "mm:1");
        assert_eq!(a.status, "NEW");
    }

    #[test]
    fn find_contract_locates_symbol() {
        let resp: ExchangeInfoResponse =
            serde_json::from_str(r#"{"symbols":[{"symbol":"BTCUSDT"},{"symbol":"ETHUSDC"}]}"#)
                .unwrap();
        // ETHUSDC 缺过滤器，应报错而不是返回残缺规则
        assert!(find_contract(&resp, "ETHUSDC").is_err());
        assert!(find_contract(&resp, "NOPE").is_err());
    }

    #[test]
    fn parse_all_reports_individual_failures() {
        let resp: ExchangeInfoResponse =
            serde_json::from_str(r#"{"symbols":[{"symbol":"ETHUSDC"},{"symbol":"BAD"}]}"#).unwrap();
        let (map, errors) = parse_all_contracts(&resp);
        assert!(map.is_empty(), "两条都缺过滤器");
        assert_eq!(errors.len(), 2, "每个失败都要被报告，不能静默跳过");
    }
}
