use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
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
    safety: LiveSafety,
    available_collateral: rust_decimal::Decimal,
    rules: LiveOrderRules,
    submitted: HashMap<String, MakerOrder>,
    protection_pairs: HashMap<String, String>,
    last_event_client_order_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LiveOrderRules {
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub min_qty: Decimal,
    pub min_notional: Decimal,
    pub take_profit_pct: Decimal,
}

#[derive(Default)]
pub struct LiveSafety {
    user_stream_connected: bool,
    account_reconciled: bool,
    armed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveReadiness {
    pub mode: ExecutionMode,
    pub endpoint: String,
    pub endpoint_allowed: bool,
    pub api_key_configured: bool,
    pub api_secret_configured: bool,
    pub can_create_runtime: bool,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveStatus {
    pub runtime_created: bool,
    pub user_stream_connected: bool,
    pub account_reconciled: bool,
    pub armed: bool,
    pub unresolved_order_ids: Vec<String>,
    #[serde(with = "rust_decimal::serde::str")]
    pub available_collateral: Decimal,
    pub message: String,
}

pub fn readiness(mode: ExecutionMode) -> LiveReadiness {
    let endpoint = std::env::var("RUST_CRYPTO_BINANCE_BASE_URL")
        .unwrap_or_else(|_| "https://testnet.binancefuture.com".to_string());
    let endpoint_allowed = network_for_endpoint(&endpoint).is_ok();
    let api_key_configured = std::env::var("RUST_CRYPTO_BINANCE_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let api_secret_configured = std::env::var("RUST_CRYPTO_BINANCE_API_SECRET")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let can_create_runtime = mode == ExecutionMode::Live
        && endpoint_allowed
        && api_key_configured
        && api_secret_configured;
    let message = if mode == ExecutionMode::Paper {
        "当前为 PAPER 模式，真实执行器未启动".to_string()
    } else if !endpoint_allowed {
        "Binance endpoint 不在允许列表中".to_string()
    } else if !api_key_configured || !api_secret_configured {
        "LIVE 模式缺少 Binance 凭据".to_string()
    } else {
        "可以创建 LIVE runtime，但仍需用户流连接、账户对账和显式 arm".to_string()
    };
    LiveReadiness {
        mode,
        endpoint,
        endpoint_allowed,
        api_key_configured,
        api_secret_configured,
        can_create_runtime,
        message,
    }
}

impl LiveSafety {
    pub fn mark_user_stream_connected(&mut self) {
        self.user_stream_connected = true;
        self.account_reconciled = false;
        self.armed = false;
    }

    pub fn mark_account_reconciled(&mut self) {
        self.account_reconciled = true;
        self.armed = false;
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }

    pub fn mark_disconnected(&mut self) {
        self.user_stream_connected = false;
        self.account_reconciled = false;
        self.armed = false;
    }

    pub fn arm(&mut self) -> Result<(), &'static str> {
        if !self.user_stream_connected {
            return Err("用户数据流尚未连接，不能进入 LIVE");
        }
        if !self.account_reconciled {
            return Err("账户尚未对账，不能进入 LIVE");
        }
        self.armed = true;
        Ok(())
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn user_stream_connected(&self) -> bool {
        self.user_stream_connected
    }

    pub fn account_reconciled(&self) -> bool {
        self.account_reconciled
    }
}

impl LiveRuntime {
    pub fn from_env(symbol: String, mode: ExecutionMode, rules: LiveOrderRules) -> Result<Self> {
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
            safety: LiveSafety::default(),
            available_collateral: rust_decimal::Decimal::ZERO,
            rules,
            submitted: HashMap::new(),
            protection_pairs: HashMap::new(),
            last_event_client_order_id: None,
        })
    }

    pub async fn connect(&mut self) -> Result<()> {
        self.user_stream.open().await?;
        self.socket = Some(self.user_stream.connect().await?);
        self.safety.mark_user_stream_connected();
        Ok(())
    }

    pub async fn keepalive(&self) -> Result<()> {
        self.user_stream.keepalive().await
    }

    pub async fn close(&mut self) -> Result<()> {
        self.socket = None;
        self.safety.mark_disconnected();
        self.user_stream.close().await
    }

    pub async fn reconcile_account(&mut self, symbol: &str, margin_asset: &str) -> Result<()> {
        let account = self.execution.account().await?;
        self.available_collateral = available_asset_balance(&account, margin_asset)?;
        ensure_flat_position(&account, symbol)?;
        self.safety.mark_account_reconciled();
        Ok(())
    }

    pub fn available_collateral(&self) -> rust_decimal::Decimal {
        self.available_collateral
    }

    pub fn submitted_orders(&self) -> Vec<MakerOrder> {
        self.submitted.values().cloned().collect()
    }

    pub async fn restore_orders(
        &mut self,
        orders: &[MakerOrder],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        for order in orders {
            self.reconciler
                .register(order.client_order_id.clone(), order.quantity, now)
                .map_err(anyhow::Error::msg)?;
            self.submitted
                .insert(order.client_order_id.clone(), order.clone());
        }
        for order in orders {
            let action = self.reconcile_order(&order.client_order_id, now).await?;
            match action {
                ReconcileAction::MarkFilled => {
                    self.submit_protection_for(&order.client_order_id, now)
                        .await?;
                }
                ReconcileAction::Alert => {
                    bail!("LIVE 重启恢复订单对账异常：{}", order.client_order_id);
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn arm(&mut self) -> Result<()> {
        self.safety.arm().map_err(anyhow::Error::msg)
    }

    pub fn disarm(&mut self) {
        self.safety.disarm();
    }

    pub async fn submit(
        &mut self,
        order: MakerOrder,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Value> {
        if !self.safety.is_armed() {
            bail!("LIVE 安全闸门未 arm，拒绝提交订单");
        }
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
                self.submitted.insert(order.client_order_id.clone(), order);
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
            self.safety.mark_disconnected();
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
        self.last_event_client_order_id = Some(event.client_order_id.clone());
        self.reconciler
            .apply_event(&event)
            .map_err(anyhow::Error::msg)
    }

    pub fn reconnect_queries(&self) -> Vec<ReconcileAction> {
        self.reconciler.on_reconnect()
    }

    pub async fn reconcile_order(
        &mut self,
        client_order_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ReconcileAction> {
        let response = self.execution.query_order(client_order_id).await?;
        let action = self
            .reconciler
            .apply_query_response(client_order_id, &response, now)
            .map_err(anyhow::Error::msg)?;
        self.last_event_client_order_id = Some(client_order_id.to_string());
        Ok(action)
    }

    pub fn unresolved_order_ids(&self) -> Vec<String> {
        self.reconciler.unresolved_client_order_ids()
    }

    pub fn tracks_order(&self, client_order_id: &str) -> bool {
        self.reconciler.get(client_order_id).is_some()
    }

    pub fn last_event_client_order_id(&self) -> Option<&str> {
        self.last_event_client_order_id.as_deref()
    }

    pub async fn cancel_expired_entries(
        &mut self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let ids: Vec<String> = self
            .submitted
            .iter()
            .filter(|(client_id, order)| {
                order.purpose == crate::execution::OrderPurpose::Entry
                    && order.expires_at.is_some_and(|expires_at| expires_at <= now)
                    && self
                        .reconciler
                        .get(client_id.as_str())
                        .is_some_and(|tracked| {
                            !matches!(
                                tracked.state,
                                crate::order_state::RemoteOrderState::Filled
                                    | crate::order_state::RemoteOrderState::Canceled
                                    | crate::order_state::RemoteOrderState::Rejected
                                    | crate::order_state::RemoteOrderState::Expired
                            )
                        })
            })
            .map(|(client_id, _)| client_id.clone())
            .collect();
        for client_id in ids {
            self.execution.cancel_order(&client_id).await?;
            let action = self.reconcile_order(&client_id, now).await?;
            if action == ReconcileAction::Alert {
                bail!("过期 LIVE 开仓单撤单后对账异常");
            }
        }
        Ok(())
    }

    pub async fn cancel_open_entries(&mut self, now: chrono::DateTime<chrono::Utc>) -> Result<()> {
        let ids: Vec<String> = self
            .submitted
            .iter()
            .filter(|(client_id, order)| {
                order.purpose == crate::execution::OrderPurpose::Entry
                    && self
                        .reconciler
                        .get(client_id.as_str())
                        .is_some_and(|tracked| {
                            !matches!(
                                tracked.state,
                                crate::order_state::RemoteOrderState::Filled
                                    | crate::order_state::RemoteOrderState::Canceled
                                    | crate::order_state::RemoteOrderState::Rejected
                                    | crate::order_state::RemoteOrderState::Expired
                            )
                        })
            })
            .map(|(client_id, _)| client_id.clone())
            .collect();
        for client_id in ids {
            self.execution.cancel_order(&client_id).await?;
            let action = self.reconcile_order(&client_id, now).await?;
            if action == ReconcileAction::Alert {
                bail!("LIVE 开仓单撤单后对账异常：{client_id}");
            }
        }
        Ok(())
    }

    pub async fn submit_protection_for(
        &mut self,
        client_order_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let entry = self
            .submitted
            .get(client_order_id)
            .cloned()
            .context("成交订单不在 LIVE runtime 记录中")?;
        if entry.purpose != crate::execution::OrderPurpose::Entry {
            return Ok(());
        }
        let tracked = self
            .reconciler
            .get(client_order_id)
            .context("成交订单未登记")?;
        if tracked.state != crate::order_state::RemoteOrderState::Filled {
            return Ok(());
        }
        let entry_price = if tracked.average_price > Decimal::ZERO {
            tracked.average_price
        } else {
            entry.price
        };
        let stop_price = entry.stop_price.context("LIVE 入场订单缺少冻结止损价")?;
        let ratio = self.rules.take_profit_pct / Decimal::from(100);
        let target = match entry.side {
            crate::model::Side::Buy => crate::model::quantize_up(
                entry_price * (Decimal::ONE + ratio),
                self.rules.tick_size,
            ),
            crate::model::Side::Sell => crate::model::quantize_down(
                entry_price * (Decimal::ONE - ratio),
                self.rules.tick_size,
            ),
        };
        let exit_side = match entry.side {
            crate::model::Side::Buy => crate::model::Side::Sell,
            crate::model::Side::Sell => crate::model::Side::Buy,
        };
        let stop_id = format!("{}:stop", client_order_id);
        let target_id = format!("{}:tp", client_order_id);
        let mut orders = Vec::new();
        if !self.submitted.contains_key(&stop_id) {
            orders.push(MakerOrder {
                client_order_id: stop_id.clone(),
                purpose: crate::execution::OrderPurpose::StopLoss,
                side: exit_side,
                quantity: tracked.filled_quantity,
                price: stop_price,
                stop_price: None,
                expires_at: None,
            });
        }
        if !self.submitted.contains_key(&target_id) {
            orders.push(MakerOrder {
                client_order_id: target_id.clone(),
                purpose: crate::execution::OrderPurpose::TakeProfit,
                side: exit_side,
                quantity: tracked.filled_quantity,
                price: target,
                stop_price: None,
                expires_at: None,
            });
        }
        for order in orders {
            order
                .validate(
                    self.rules.tick_size,
                    self.rules.step_size,
                    self.rules.min_qty,
                    self.rules.min_notional,
                )
                .map_err(anyhow::Error::msg)?;
            self.submit(order, now).await?;
        }
        self.protection_pairs.insert(stop_id, target_id);
        Ok(())
    }

    pub async fn cancel_protection_sibling(&mut self, client_order_id: &str) -> Result<()> {
        let sibling = self
            .protection_pairs
            .get(client_order_id)
            .cloned()
            .or_else(|| {
                self.protection_pairs
                    .iter()
                    .find_map(|(left, right)| (right == client_order_id).then(|| left.clone()))
            });
        let Some(sibling) = sibling else {
            return Ok(());
        };
        self.execution.cancel_order(&sibling).await?;
        Ok(())
    }

    pub fn order(&self, client_order_id: &str) -> Option<&crate::order_state::TrackedOrder> {
        self.reconciler.get(client_order_id)
    }

    pub fn status(&self) -> LiveStatus {
        let unresolved_order_ids = self.unresolved_order_ids();
        let message = if !self.safety.user_stream_connected() {
            "用户数据流未连接，LIVE 已停用".to_string()
        } else if !self.safety.account_reconciled() {
            "用户数据流已连接，等待账户对账".to_string()
        } else if !self.safety.is_armed() {
            "账户已对账，等待显式 arm".to_string()
        } else if !unresolved_order_ids.is_empty() {
            "LIVE 已 arm，存在待对账订单".to_string()
        } else {
            "LIVE 已 arm，当前没有待对账订单".to_string()
        };
        LiveStatus {
            runtime_created: true,
            user_stream_connected: self.safety.user_stream_connected(),
            account_reconciled: self.safety.account_reconciled(),
            armed: self.safety.is_armed(),
            unresolved_order_ids,
            available_collateral: self.available_collateral,
            message,
        }
    }
}

fn available_asset_balance(account: &Value, asset: &str) -> Result<rust_decimal::Decimal> {
    let assets = account["assets"]
        .as_array()
        .context("Binance 账户响应缺少 assets")?;
    let item = assets
        .iter()
        .find(|item| item["asset"].as_str() == Some(asset))
        .with_context(|| format!("Binance 账户没有结算资产 {asset}"))?;
    let value = item["availableBalance"]
        .as_str()
        .with_context(|| format!("Binance 账户 {asset} availableBalance 无效"))?;
    let balance = rust_decimal::Decimal::from_str_exact(value)
        .with_context(|| format!("Binance 账户 {asset} availableBalance 不是 Decimal"))?;
    if balance < rust_decimal::Decimal::ZERO {
        bail!("Binance 账户 {asset} availableBalance 为负数");
    }
    Ok(balance)
}

fn ensure_flat_position(account: &Value, symbol: &str) -> Result<()> {
    let positions = account["positions"]
        .as_array()
        .context("Binance 账户响应缺少 positions，拒绝 LIVE")?;
    let position = positions
        .iter()
        .find(|item| item["symbol"].as_str() == Some(symbol));
    let Some(position) = position else {
        bail!("Binance 账户没有交易对 {symbol} 的持仓记录，拒绝 LIVE");
    };
    let amount = rust_decimal::Decimal::from_str_exact(
        position["positionAmt"]
            .as_str()
            .with_context(|| format!("Binance {symbol} positionAmt 无效"))?,
    )
    .with_context(|| format!("Binance {symbol} positionAmt 不是 Decimal"))?;
    if amount != rust_decimal::Decimal::ZERO {
        bail!("Binance {symbol} 存在未恢复持仓 {amount}，拒绝 ARM");
    }
    Ok(())
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
        let result = LiveRuntime::from_env(
            "ETHUSDC".to_string(),
            ExecutionMode::Paper,
            LiveOrderRules {
                tick_size: Decimal::ONE,
                step_size: Decimal::ONE,
                min_qty: Decimal::ONE,
                min_notional: Decimal::ONE,
                take_profit_pct: Decimal::ONE,
            },
        );
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

    #[test]
    fn live_safety_requires_stream_and_account_reconciliation() {
        let mut safety = LiveSafety::default();
        assert!(safety.arm().is_err());
        safety.mark_user_stream_connected();
        assert!(safety.arm().is_err());
        safety.mark_account_reconciled();
        safety.arm().unwrap();
        assert!(safety.is_armed());
        safety.disarm();
        assert!(!safety.is_armed());
    }

    #[test]
    fn paper_readiness_never_reports_live_creation() {
        let result = readiness(ExecutionMode::Paper);
        assert!(!result.can_create_runtime);
        assert_eq!(result.mode, ExecutionMode::Paper);
    }

    #[test]
    fn account_reconciliation_keeps_settlement_asset_explicit() {
        let value = serde_json::json!({
            "assets": [
                {"asset": "USDT", "availableBalance": "12.50"},
                {"asset": "USDC", "availableBalance": "3.25"}
            ]
        });
        assert_eq!(
            available_asset_balance(&value, "USDC").unwrap(),
            rust_decimal::Decimal::new(325, 2)
        );
        assert!(available_asset_balance(&value, "BTC").is_err());
    }

    #[test]
    fn account_reconciliation_rejects_unrecovered_position() {
        let value = serde_json::json!({
            "assets": [{"asset": "USDC", "availableBalance": "3.25"}],
            "positions": [{"symbol": "ETHUSDC", "positionAmt": "0.010"}]
        });
        assert!(ensure_flat_position(&value, "ETHUSDC").is_err());
        let flat = serde_json::json!({
            "assets": [{"asset": "USDC", "availableBalance": "3.25"}],
            "positions": [{"symbol": "ETHUSDC", "positionAmt": "0"}]
        });
        assert!(ensure_flat_position(&flat, "ETHUSDC").is_ok());
    }
}
