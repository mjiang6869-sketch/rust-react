//! 币安 USDⓈ-M REST 客户端。
//!
//! # 职责边界
//!
//! 本模块只做"把请求发出去、把响应解析成领域类型"。**它不做任何交易决策**，
//! 也不管理订单状态——那是 `domain::OrderBookState` 的职责。
//!
//! # 安全
//!
//! - 每个签名请求前都校验 endpoint 白名单
//! - 密钥只在构造请求头时被访问，不进入日志或错误信息
//! - 超时与重试策略在错误分类层面表达（见 `classify_api_error`），
//!   而不是在客户端里盲目重试——那可能重复下单

use std::time::Duration;

use chrono::Utc;
use domain::ServiceMode;
use rust_decimal::Decimal;

use crate::binance::{
    AcceptedOrder, AccountFeeResponse, ContractSpec, ExchangeInfoResponse, OrderRequest,
    OrderResponse, classify_api_error, find_contract, parse_all_contracts,
};
use crate::error::ExchangeError;
use crate::signing::{Credentials, endpoint_allowed};

/// 生产环境 base URL。
pub const PRODUCTION_URL: &str = "https://fapi.binance.com";
/// 测试网 base URL。
pub const TESTNET_URL: &str = "https://testnet.binancefuture.com";

/// 默认接收窗口（毫秒）。请求签名的时间容差。
const DEFAULT_RECV_WINDOW_MS: u64 = 5_000;

/// 运行模式。
///
/// `Paper` 模式下客户端**不可构造**——这样"模拟盘误发真实订单"在类型层面
/// 就不可能出现。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 实盘，需要凭据。
    Live,
}

/// 币安 REST 客户端。
pub struct BinanceClient {
    http: reqwest::Client,
    base: String,
    creds: Option<Credentials>,
    recv_window_ms: u64,
}

impl BinanceClient {
    /// 构造只读客户端（不需要凭据）。用于拉取公开的合约规则。
    pub fn public(base: &str) -> Result<Self, ExchangeError> {
        endpoint_allowed(base)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::Fatal(format!("构造 HTTP 客户端失败：{e}")))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            creds: None,
            recv_window_ms: DEFAULT_RECV_WINDOW_MS,
        })
    }

    /// 构造签名客户端。
    ///
    /// `mode` 参数存在的意义是让调用点显式声明意图。将来加入 `Paper` 时，
    /// 这里会是"模拟盘无法构造签名客户端"的落点。
    pub fn signed(base: &str, creds: Credentials, mode: Mode) -> Result<Self, ExchangeError> {
        let Mode::Live = mode;
        endpoint_allowed(base)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::Fatal(format!("构造 HTTP 客户端失败：{e}")))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            creds: Some(creds),
            recv_window_ms: DEFAULT_RECV_WINDOW_MS,
        })
    }

    /// 从环境变量构造签名客户端。
    ///
    /// 环境变量：`RUST_CRYPTO_BINANCE_API_KEY` / `RUST_CRYPTO_BINANCE_API_SECRET`
    /// / `RUST_CRYPTO_BINANCE_BASE_URL`（默认测试网）。
    pub fn from_env() -> Result<Self, ExchangeError> {
        let creds = Credentials::from_env().ok_or_else(|| {
            ExchangeError::Fatal(
                "缺少 API 凭据。需要设置 RUST_CRYPTO_BINANCE_API_KEY 与 \
                 RUST_CRYPTO_BINANCE_API_SECRET。"
                    .into(),
            )
        })?;
        let base = std::env::var("RUST_CRYPTO_BINANCE_BASE_URL")
            .unwrap_or_else(|_| TESTNET_URL.to_string());
        Self::signed(&base, creds, Mode::Live)
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// 是否具备签名能力。
    pub fn is_signed(&self) -> bool {
        self.creds.is_some()
    }

    fn require_creds(&self) -> Result<&Credentials, ExchangeError> {
        self.creds
            .as_ref()
            .ok_or_else(|| ExchangeError::Fatal("该操作需要 API 凭据，但客户端是只读的".into()))
    }

    /// 发送签名请求（POST，参数放在查询串里）。
    async fn post_signed(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<String, ExchangeError> {
        let creds = self.require_creds()?;
        let url = format!("{}{path}", self.base);
        // 每个签名请求都校验白名单——不依赖构造时的检查，避免
        // base 被后续修改绕过。
        endpoint_allowed(&url)?;

        let query = creds.sign_params(params, self.recv_window_ms)?;
        let full = format!("{url}?{query}");

        let resp = self
            .http
            .post(&full)
            .header("X-MBX-APIKEY", &creds.api_key)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    // 超时 = 状态未知。绝不能当成"未送达"直接重发——
                    // 订单可能已经成交了。
                    ExchangeError::Unknown(format!("请求超时：{e}"))
                } else if e.is_connect() {
                    ExchangeError::Definitive(format!("连接失败：{e}"))
                } else {
                    ExchangeError::Unknown(format!("请求失败：{e}"))
                }
            })?;

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(classify_api_error(status, &body))
        }
    }

    /// 发送签名 GET 请求。
    async fn get_signed(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<String, ExchangeError> {
        let creds = self.require_creds()?;
        let url = format!("{}{path}", self.base);
        endpoint_allowed(&url)?;
        let query = creds.sign_params(params, self.recv_window_ms)?;

        let resp = self
            .http
            .get(format!("{url}?{query}"))
            .header("X-MBX-APIKEY", &creds.api_key)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ExchangeError::Unknown(format!("请求超时：{e}"))
                } else {
                    ExchangeError::Definitive(format!("请求失败：{e}"))
                }
            })?;

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(classify_api_error(status, &body))
        }
    }

    /// 发送签名 DELETE 请求（撤单用）。
    async fn delete_signed(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<String, ExchangeError> {
        let creds = self.require_creds()?;
        let url = format!("{}{path}", self.base);
        endpoint_allowed(&url)?;
        let query = creds.sign_params(params, self.recv_window_ms)?;

        let resp = self
            .http
            .delete(format!("{url}?{query}"))
            .header("X-MBX-APIKEY", &creds.api_key)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    // 撤单超时同样不能重试——撤单可能已经生效，
                    // 重试会得到 -2011 或被误导。
                    ExchangeError::Unknown(format!("撤单请求超时：{e}"))
                } else {
                    ExchangeError::Definitive(format!("撤单请求失败：{e}"))
                }
            })?;

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(classify_api_error(status, &body))
        }
    }

    /// 发送公开 GET 请求（无需签名）。
    async fn get_public(&self, path: &str, query: &str) -> Result<String, ExchangeError> {
        let url = if query.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        let resp = self.http.get(&url).send().await.map_err(|e| {
            if e.is_timeout() {
                ExchangeError::Unknown(format!("请求超时：{e}"))
            } else {
                ExchangeError::Definitive(format!("请求失败：{e}"))
            }
        })?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(classify_api_error(status, &body))
        }
    }

    /// 拉取全部合约规则。
    ///
    /// 返回 `(合约表, 解析失败的合约)`.  解析失败**不静默丢弃**——币安改字段
    /// 时必须让人看到，否则会以为某个合约不存在。
    pub async fn exchange_info(
        &self,
    ) -> Result<
        (
            std::collections::BTreeMap<String, ContractSpec>,
            Vec<String>,
        ),
        ExchangeError,
    > {
        let body = self.get_public("/fapi/v1/exchangeInfo", "").await?;
        let resp: ExchangeInfoResponse = serde_json::from_str(&body)
            .map_err(|e| ExchangeError::Fatal(format!("exchangeInfo 解析失败：{e}")))?;
        Ok(parse_all_contracts(&resp))
    }

    /// 拉取单个合约规则。
    pub async fn contract(&self, symbol: &str) -> Result<ContractSpec, ExchangeError> {
        let body = self.get_public("/fapi/v1/exchangeInfo", "").await?;
        let resp: ExchangeInfoResponse = serde_json::from_str(&body)
            .map_err(|e| ExchangeError::Fatal(format!("exchangeInfo 解析失败：{e}")))?;
        find_contract(&resp, symbol)
    }

    /// 读取账户的实际费率。
    ///
    /// **这是唯一能让费率为"权威"的来源。** 零费率活动是策略 edge 的全部来源，
    /// 必须与账户对账而不是假设。
    pub async fn account_fees(&self) -> Result<domain::FeeSchedule, ExchangeError> {
        let body = self.get_signed("/fapi/v2/account", vec![]).await?;
        let resp: AccountFeeResponse = serde_json::from_str(&body)
            .map_err(|e| ExchangeError::Fatal(format!("账户信息解析失败：{e}")))?;
        Ok(crate::binance::fee_schedule_from_account(&resp, Utc::now()))
    }

    /// 账户可用余额（按资产）。
    ///
    /// 多资产模式下 USDT 可以为 USDC 合约提供保证金，但**盈亏仍结算在
    /// 合约的 margin_asset**，所以两者必须分开读取、绝不合并。
    pub async fn available_balance(&self, asset: &str) -> Result<Decimal, ExchangeError> {
        let body = self.get_signed("/fapi/v2/balance", vec![]).await?;
        Ok(parse_available_balance(&body, asset))
    }

    /// 提交订单。
    ///
    /// `newOrderRespType=RESULT` 让币安直接返回成交状态——比 ACK 少一次查询，
    /// 但**超时仍归为 Unknown**，因为响应可能只是没收到。
    pub async fn submit_order(&self, req: &OrderRequest) -> Result<AcceptedOrder, ExchangeError> {
        let mut params: Vec<(&str, String)> = vec![
            ("symbol", req.symbol.clone()),
            ("side", req.side.clone()),
            ("type", req.order_type.clone()),
            ("quantity", req.quantity.clone()),
            ("newClientOrderId", req.client_order_id.clone()),
            ("newOrderRespType", "RESULT".to_string()),
        ];
        if let Some(p) = &req.price {
            params.push(("price", p.clone()));
        }
        if let Some(t) = &req.time_in_force {
            params.push(("timeInForce", t.clone()));
        }
        if let Some(r) = req.reduce_only {
            params.push(("reduceOnly", r.to_string()));
        }
        if let Some(s) = &req.stop_price {
            params.push(("stopPrice", s.clone()));
        }
        if let Some(w) = &req.working_type {
            params.push(("workingType", w.clone()));
        }
        if let Some(pp) = req.price_protect {
            params.push(("priceProtect", pp.to_string()));
        }
        if let Some(g) = req.good_till_date {
            params.push(("goodTillDate", g.to_string()));
        }

        let body = self.post_signed("/fapi/v1/order", params).await?;
        let resp: OrderResponse = serde_json::from_str(&body)
            .map_err(|e| ExchangeError::Unknown(format!("下单响应无法解析：{e}；原文：{body}")))?;
        AcceptedOrder::try_from(resp)
    }

    /// 查询订单。
    ///
    /// **这是 `Unknown` 状态的唯一出口。** post-only 被静默拒绝时币安返回
    /// 错误码 -2013（订单不存在），调用方应据此判定"未成交且已消失"。
    pub async fn query_order(
        &self,
        symbol: &str,
        client_order_id: &str,
    ) -> Result<Option<AcceptedOrder>, ExchangeError> {
        let params = vec![
            ("symbol", symbol.to_string()),
            ("origClientOrderId", client_order_id.to_string()),
        ];
        match self.get_signed("/fapi/v1/order", params).await {
            Ok(body) => {
                let resp: OrderResponse = serde_json::from_str(&body)
                    .map_err(|e| ExchangeError::Unknown(format!("订单查询响应无法解析：{e}")))?;
                Ok(Some(AcceptedOrder::try_from(resp)?))
            }
            Err(ExchangeError::Definitive(msg)) if msg.contains("-2013") => {
                // 订单不存在。对 post-only 而言这是常态（被拒的单不记入历史），
                // 不是错误。
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// 撤销订单。
    ///
    /// 撤单失败有三种可能，处理方式不同：
    /// - `-2011` 订单不存在：可能已经成交或被拒 → 需要查询确认
    /// - `Unknown` 超时：**必须先查询**，不能重试
    pub async fn cancel_order(
        &self,
        symbol: &str,
        client_order_id: &str,
    ) -> Result<(), ExchangeError> {
        let params = vec![
            ("symbol", symbol.to_string()),
            ("origClientOrderId", client_order_id.to_string()),
        ];
        match self.delete_signed("/fapi/v1/order", params).await {
            Ok(_) => Ok(()),
            Err(ExchangeError::Definitive(msg)) if msg.contains("-2011") => {
                // 订单不存在 —— 视为已撤销，但调用方应查询确认真实状态
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// 从 `/fapi/v2/balance` 响应里取某资产的可用余额。
///
/// 找不到该资产时返回 0 而不是报错——账户里可能确实没有该资产。
/// 但**不会把多个资产相加**。
pub fn parse_available_balance(body: &str, asset: &str) -> Decimal {
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(default)]
        asset: String,
        #[serde(default, rename = "availableBalance")]
        available: String,
    }
    let Ok(entries) = serde_json::from_str::<Vec<Entry>>(body) else {
        return Decimal::ZERO;
    };
    entries
        .iter()
        .find(|e| e.asset == asset)
        .and_then(|e| e.available.parse::<Decimal>().ok())
        .unwrap_or(Decimal::ZERO)
}

/// 把领域层的服务模式映射到交易所模式。
///
/// 模拟盘没有对应的交易所模式——返回 `None` 表示不需要交易所客户端。
pub fn exchange_mode_for(service: ServiceMode) -> Option<Mode> {
    match service {
        ServiceMode::Paper => None,
        ServiceMode::Live => Some(Mode::Live),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_client_accepts_official_endpoints() {
        assert!(BinanceClient::public(PRODUCTION_URL).is_ok());
        assert!(BinanceClient::public(TESTNET_URL).is_ok());
    }

    /// 白名单必须在构造时就拦截，不能等到发请求。
    ///
    /// `BinanceClient` 刻意不实现 `Debug`（它可能持有凭据），所以这里用
    /// `match` 而非 `unwrap_err`。
    #[test]
    fn public_client_rejects_unknown_hosts() {
        match BinanceClient::public("https://evil.example") {
            Ok(_) => panic!("非白名单主机不应被接受——签名请求会把凭据发过去"),
            Err(e) => {
                assert!(e.is_fatal(), "白名单失败是配置问题，属于致命错误：{e:?}");
                assert!(
                    e.to_string().contains("evil.example"),
                    "错误应指出主机：{e}"
                );
            }
        }
    }

    #[test]
    fn signed_client_requires_credentials() {
        let c = BinanceClient::public(PRODUCTION_URL).unwrap();
        assert!(!c.is_signed());
        // 只读客户端做签名操作必须报错而不是静默失败
        assert!(c.require_creds().is_err());
    }

    /// trailing slash 要被规范化，否则拼接路径会产生双斜杠。
    #[test]
    fn base_url_trailing_slash_is_normalized() {
        let c = BinanceClient::public("https://fapi.binance.com/").unwrap();
        assert_eq!(c.base_url(), "https://fapi.binance.com");
    }

    /// **余额按资产分开读取，绝不合并。**
    /// 多资产模式下 USDT 可为 USDC 合约提供保证金，但盈亏结算在 USDC。
    #[test]
    fn available_balance_is_read_per_asset() {
        let body = r#"[
            {"asset":"USDC","availableBalance":"1234.56"},
            {"asset":"USDT","availableBalance":"7890.12"}
        ]"#;
        assert_eq!(parse_available_balance(body, "USDC"), dec!(1234.56));
        assert_eq!(parse_available_balance(body, "USDT"), dec!(7890.12));
        assert_eq!(
            parse_available_balance(body, "BNB"),
            Decimal::ZERO,
            "不存在的资产返回 0，不是两者之和"
        );
    }

    #[test]
    fn balance_parsing_tolerates_malformed_input() {
        assert_eq!(parse_available_balance("not json", "USDC"), Decimal::ZERO);
        assert_eq!(parse_available_balance("[]", "USDC"), Decimal::ZERO);
        assert_eq!(
            parse_available_balance(r#"[{"asset":"USDC","availableBalance":"abc"}]"#, "USDC"),
            Decimal::ZERO
        );
    }

    /// 模拟盘不需要交易所客户端——这个映射让"模拟盘误发真实订单"
    /// 在调用点就暴露。
    #[test]
    fn paper_mode_has_no_exchange_client() {
        assert!(exchange_mode_for(ServiceMode::Paper).is_none());
        assert!(exchange_mode_for(ServiceMode::Live).is_some());
    }

    use rust_decimal_macros::dec;
}
