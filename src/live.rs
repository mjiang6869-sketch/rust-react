use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::Value;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::binance::BinanceExecution;
use crate::execution::MakerOrder;
use crate::order_state::{OrderReconciler, ReconcileAction};
use crate::paper::ExecutionMode;
use crate::user_stream::{BinanceNetwork, EventDeduper, UserDataStream, parse_order_trade_update};

pub struct LiveRuntime {
    execution: BinanceExecution,
    user_stream: UserDataStream,
    socket: Option<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
    reconciler: OrderReconciler,
    deduper: EventDeduper,
}

impl LiveRuntime {
    pub fn from_env(symbol: String, mode: ExecutionMode) -> Result<Self> {
        if mode != ExecutionMode::Live {
            bail!("LIVE runtime 只能在显式 LIVE 模式创建");
        }
        let api_key = std::env::var("RUST_CRYPTO_BINANCE_API_KEY")
            .context("LIVE runtime 缺少 Binance API Key")?;
        let network = network_from_env()?;
        Ok(Self {
            execution: BinanceExecution::from_env(symbol.clone(), mode)?,
            user_stream: UserDataStream::new(api_key, network)?,
            socket: None,
            reconciler: OrderReconciler::new(symbol),
            deduper: EventDeduper::new(4_096)?,
        })
    }

    pub async fn connect(&mut self) -> Result<()> {
        self.user_stream.open().await?;
        self.socket = Some(self.user_stream.connect().await?);
        Ok(())
    }

    pub async fn keepalive(&self) -> Result<()> {
        self.user_stream.keepalive().await
    }

    pub async fn close(&mut self) -> Result<()> {
        self.socket = None;
        self.user_stream.close().await
    }

    pub async fn submit(
        &mut self,
        order: MakerOrder,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Value> {
        self.reconciler
            .register(order.client_order_id.clone(), order.quantity, now)
            .map_err(anyhow::Error::msg)?;
        match self.execution.submit_post_only(&order).await {
            Ok(response) => {
                let exchange_order_id = response["orderId"]
                    .as_i64()
                    .map(|value| value.to_string())
                    .or_else(|| response["orderId"].as_str().map(str::to_string))
                    .context("Binance 下单响应缺少 orderId")?;
                self.reconciler
                    .submit_ack(&order.client_order_id, exchange_order_id, now)
                    .map_err(anyhow::Error::msg)?;
                Ok(response)
            }
            Err(error) => {
                let _ = self.reconciler.submit_unknown(&order.client_order_id, now);
                Err(error).context("Maker 订单提交结果未知，已转入查询流程")
            }
        }
    }

    pub async fn next_reconcile_action(&mut self) -> Result<Option<ReconcileAction>> {
        let socket = self.socket.as_mut().context("LIVE 用户数据流尚未连接")?;
        let Some(message) = socket.next().await else {
            self.socket = None;
            return Ok(Some(ReconcileAction::Alert));
        };
        let message = message?;
        let Message::Text(text) = message else {
            return Ok(None);
        };
        let Some(event) = parse_order_trade_update(&text)? else {
            return Ok(None);
        };
        if !self.deduper.accept(&event) {
            return Ok(None);
        }
        self.reconciler
            .apply_event(&event)
            .map_err(anyhow::Error::msg)
    }

    pub fn reconnect_queries(&self) -> Vec<ReconcileAction> {
        self.reconciler.on_reconnect()
    }

    pub fn order(&self, client_order_id: &str) -> Option<&crate::order_state::TrackedOrder> {
        self.reconciler.get(client_order_id)
    }
}

fn network_from_env() -> Result<BinanceNetwork> {
    let endpoint = std::env::var("RUST_CRYPTO_BINANCE_BASE_URL")
        .unwrap_or_else(|_| "https://testnet.binancefuture.com".to_string());
    network_for_endpoint(&endpoint)
}

fn network_for_endpoint(endpoint: &str) -> Result<BinanceNetwork> {
    match endpoint {
        "https://fapi.binance.com" => Ok(BinanceNetwork::Production),
        "https://testnet.binancefuture.com" => Ok(BinanceNetwork::Testnet),
        _ => bail!("Binance endpoint 不在允许列表中"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paper_mode_before_reading_credentials() {
        let result = LiveRuntime::from_env("ETHUSDC".to_string(), ExecutionMode::Paper);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_unknown_network_without_network_access() {
        assert!(network_for_endpoint("https://example.invalid").is_err());
        assert_eq!(
            network_for_endpoint("https://testnet.binancefuture.com").unwrap(),
            BinanceNetwork::Testnet
        );
    }
}
