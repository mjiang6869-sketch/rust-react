use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::time::Duration;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const PRODUCTION_REST: &str = "https://fapi.binance.com";
const TESTNET_REST: &str = "https://testnet.binancefuture.com";
const PRODUCTION_WS: &str = "wss://fstream.binance.com/ws";
const TESTNET_WS: &str = "wss://stream.binancefuture.com/ws";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinanceNetwork {
    Production,
    Testnet,
}

impl BinanceNetwork {
    fn rest_url(self) -> &'static str {
        match self {
            Self::Production => PRODUCTION_REST,
            Self::Testnet => TESTNET_REST,
        }
    }

    fn ws_url(self) -> &'static str {
        match self {
            Self::Production => PRODUCTION_WS,
            Self::Testnet => TESTNET_WS,
        }
    }
}

pub struct UserDataStream {
    client: Client,
    api_key: String,
    network: BinanceNetwork,
    listen_key: Option<String>,
}

impl UserDataStream {
    pub fn new(api_key: String, network: BinanceNetwork) -> Result<Self> {
        if api_key.trim().is_empty() {
            bail!("用户数据流缺少 Binance API Key");
        }
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(10)).build()?,
            api_key,
            network,
            listen_key: None,
        })
    }

    pub async fn open(&mut self) -> Result<()> {
        let response: ListenKeyResponse = self
            .client
            .post(format!("{}/fapi/v1/listenKey", self.network.rest_url()))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Binance listenKey 响应无效")?;
        self.listen_key = Some(response.listen_key);
        Ok(())
    }

    pub async fn keepalive(&self) -> Result<()> {
        let listen_key = self.listen_key.as_deref().context("用户数据流尚未打开")?;
        self.client
            .put(format!("{}/fapi/v1/listenKey", self.network.rest_url()))
            .header("X-MBX-APIKEY", &self.api_key)
            .query(&[("listenKey", listen_key)])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn close(&mut self) -> Result<()> {
        let Some(listen_key) = self.listen_key.take() else {
            return Ok(());
        };
        self.client
            .delete(format!("{}/fapi/v1/listenKey", self.network.rest_url()))
            .header("X-MBX-APIKEY", &self.api_key)
            .query(&[("listenKey", listen_key)])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn connect(&self) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>> {
        let listen_key = self.listen_key.as_deref().context("用户数据流尚未打开")?;
        let (socket, _) =
            connect_async(format!("{}/{}", self.network.ws_url(), listen_key)).await?;
        Ok(socket)
    }
}

#[derive(Debug, Deserialize)]
struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    listen_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderTradeUpdate {
    pub event_time: DateTime<Utc>,
    pub symbol: String,
    pub client_order_id: String,
    pub exchange_order_id: String,
    pub status: String,
    pub execution_type: String,
    pub last_trade_id: Option<String>,
    pub last_filled_quantity: Decimal,
    pub cumulative_filled_quantity: Decimal,
    pub average_price: Decimal,
    pub reduce_only: bool,
}

pub fn parse_order_trade_update(text: &str) -> Result<Option<OrderTradeUpdate>> {
    let value: Value = serde_json::from_str(text).context("用户数据事件不是有效 JSON")?;
    if value["e"] != "ORDER_TRADE_UPDATE" {
        return Ok(None);
    }
    let order = &value["o"];
    let event_ms = value["E"].as_i64().context("用户事件时间无效")?;
    let event_time =
        chrono::DateTime::from_timestamp_millis(event_ms).context("用户事件时间越界")?;
    Ok(Some(OrderTradeUpdate {
        event_time,
        symbol: required_string(order, "s")?,
        client_order_id: required_string(order, "c")?,
        exchange_order_id: required_string(order, "i")?,
        status: required_string(order, "X")?,
        execution_type: required_string(order, "x")?,
        last_trade_id: optional_string(order, "t"),
        last_filled_quantity: decimal_string(order, "l")?,
        cumulative_filled_quantity: decimal_string(order, "z")?,
        average_price: decimal_string(order, "ap")?,
        reduce_only: order["R"].as_bool().context("订单 reduceOnly 标志无效")?,
    }))
}

fn required_string(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("用户订单字段 {key} 无效"))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_string)
}

fn decimal_string(value: &Value, key: &str) -> Result<Decimal> {
    Decimal::from_str_exact(
        value[key]
            .as_str()
            .with_context(|| format!("用户订单数值 {key} 不是字符串"))?,
    )
    .with_context(|| format!("用户订单数值 {key} 无效"))
}

#[derive(Default)]
pub struct EventDeduper {
    seen: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl EventDeduper {
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            bail!("事件去重容量必须大于零");
        }
        Ok(Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            capacity,
        })
    }

    pub fn accept(&mut self, event: &OrderTradeUpdate) -> bool {
        let trade_or_event = event
            .last_trade_id
            .as_deref()
            .unwrap_or(&event.execution_type);
        let key = format!(
            "{}:{trade_or_event}:{}",
            event.exchange_order_id, event.status
        );
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(expired) = self.order.pop_front() {
                self.seen.remove(&expired);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPDATE: &str = r#"{
      "e":"ORDER_TRADE_UPDATE","E":1727000000123,
      "o":{"s":"ETHUSDC","c":"mm-entry-1","i":"12345","X":"FILLED","x":"TRADE","t":88,"l":"0.050","z":"0.050","ap":"100.00","R":false}
    }"#;

    #[test]
    fn parses_order_update_and_deduplicates_same_trade() {
        let event = parse_order_trade_update(UPDATE).unwrap().unwrap();
        assert_eq!(event.client_order_id, "mm-entry-1");
        assert_eq!(event.cumulative_filled_quantity, Decimal::new(50, 3));
        let mut deduper = EventDeduper::new(16).unwrap();
        assert!(deduper.accept(&event));
        assert!(!deduper.accept(&event));
    }

    #[test]
    fn ignores_non_order_events_and_rejects_zero_capacity() {
        assert!(
            parse_order_trade_update(r#"{"e":"ACCOUNT_UPDATE"}"#)
                .unwrap()
                .is_none()
        );
        assert!(EventDeduper::new(0).is_err());
    }
}
