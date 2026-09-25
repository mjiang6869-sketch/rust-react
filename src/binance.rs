use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use reqwest::{Client, Method, RequestBuilder};
use serde_json::Value;
use sha2::Sha256;
use std::time::Duration;
use url::form_urlencoded::Serializer;

use crate::execution::MakerOrder;
use crate::model::Side;
use crate::paper::ExecutionMode;

const PRODUCTION_ENDPOINT: &str = "https://fapi.binance.com";
const TESTNET_ENDPOINT: &str = "https://testnet.binancefuture.com";

pub struct BinanceExecution {
    client: Client,
    symbol: String,
    api_key: String,
    api_secret: String,
    base_url: String,
    recv_window: u64,
}

impl BinanceExecution {
    pub fn from_env(symbol: String, mode: ExecutionMode) -> Result<Self> {
        if mode != ExecutionMode::Live {
            bail!("Binance 真实执行器只能在显式 LIVE 模式创建");
        }
        let base_url = std::env::var("RUST_CRYPTO_BINANCE_BASE_URL")
            .unwrap_or_else(|_| TESTNET_ENDPOINT.to_string());
        if !is_allowed_endpoint(&base_url) {
            bail!("Binance endpoint 不在允许列表中");
        }
        let api_key = std::env::var("RUST_CRYPTO_BINANCE_API_KEY")
            .context("LIVE 模式缺少 Binance API Key")?;
        let api_secret = std::env::var("RUST_CRYPTO_BINANCE_API_SECRET")
            .context("LIVE 模式缺少 Binance API Secret")?;
        if api_key.trim().is_empty() || api_secret.trim().is_empty() {
            bail!("LIVE 模式的 Binance 凭据不能为空");
        }
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(10)).build()?,
            symbol,
            api_key,
            api_secret,
            base_url: base_url.trim_end_matches('/').to_string(),
            recv_window: 5_000,
        })
    }

    pub async fn submit_post_only(&self, order: &MakerOrder) -> Result<Value> {
        let mut params = vec![
            ("symbol", self.symbol.clone()),
            ("side", side_name(order.side).to_string()),
            ("type", "LIMIT".to_string()),
            ("timeInForce", "GTX".to_string()),
            ("quantity", order.quantity.normalize().to_string()),
            ("price", order.price.normalize().to_string()),
            ("newClientOrderId", order.client_order_id.clone()),
            ("newOrderRespType", "RESULT".to_string()),
        ];
        if order.is_reduce_only() {
            params.push(("reduceOnly", "true".to_string()));
        }
        self.signed_request(Method::POST, "/fapi/v1/order", params)
            .await
    }

    pub async fn query_order(&self, client_order_id: &str) -> Result<Value> {
        self.signed_request(
            Method::GET,
            "/fapi/v1/order",
            vec![
                ("symbol", self.symbol.clone()),
                ("origClientOrderId", client_order_id.to_string()),
            ],
        )
        .await
    }

    pub async fn cancel_order(&self, client_order_id: &str) -> Result<Value> {
        self.signed_request(
            Method::DELETE,
            "/fapi/v1/order",
            vec![
                ("symbol", self.symbol.clone()),
                ("origClientOrderId", client_order_id.to_string()),
            ],
        )
        .await
    }

    pub async fn account(&self) -> Result<Value> {
        self.signed_request(Method::GET, "/fapi/v2/account", Vec::new())
            .await
    }

    async fn signed_request(
        &self,
        method: Method,
        path: &str,
        mut params: Vec<(&str, String)>,
    ) -> Result<Value> {
        params.push(("recvWindow", self.recv_window.to_string()));
        params.push((
            "timestamp",
            chrono::Utc::now().timestamp_millis().to_string(),
        ));
        let mut serializer = Serializer::new(String::new());
        for (name, value) in &params {
            serializer.append_pair(name, value);
        }
        let query = serializer.finish();
        let signature = sign(&self.api_secret, &query)?;
        let url = format!("{}{path}?{query}&signature={signature}", self.base_url);
        let request = self
            .client
            .request(method, url)
            .header("X-MBX-APIKEY", &self.api_key);
        send_json(request).await
    }
}

async fn send_json(request: RequestBuilder) -> Result<Value> {
    Ok(request.send().await?.error_for_status()?.json().await?)
}

fn sign(secret: &str, query: &str) -> Result<String> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).context("Binance 签名密钥无效")?;
    mac.update(query.as_bytes());
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

fn is_allowed_endpoint(endpoint: &str) -> bool {
    endpoint == PRODUCTION_ENDPOINT || endpoint == TESTNET_ENDPOINT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allows_known_binance_endpoints() {
        assert!(is_allowed_endpoint(PRODUCTION_ENDPOINT));
        assert!(is_allowed_endpoint(TESTNET_ENDPOINT));
        assert!(!is_allowed_endpoint("https://example.invalid"));
    }

    #[test]
    fn signs_query_without_exposing_secret() {
        let signature = sign("secret", "symbol=ETHUSDT&timestamp=1").unwrap();
        assert_eq!(signature.len(), 64);
        assert!(!signature.contains("secret"));
    }
}
