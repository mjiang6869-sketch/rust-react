use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::execution::{MakerOrder, OrderPurpose};
use crate::model::{Candle, Side, Signal, quantize_down, quantize_up};
use crate::signal::{find_reversal_signal, invalid_reason, risk_reason};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionMode {
    #[default]
    Paper,
    Live,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub symbol: String,
    pub quote_asset: String,
    pub margin_asset: String,
    pub contract_type: String,
    pub multi_assets_mode: bool,
    pub enabled: bool,
    #[serde(with = "rust_decimal::serde::str")]
    pub margin_pct: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub leverage: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub stop_pct: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub take_profit_pct: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub maker_fee_pct: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub tick_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub step_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_qty: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_notional: Decimal,
}

impl Config {
    pub fn initial(symbol: String) -> Self {
        Self {
            symbol,
            quote_asset: "USDC".to_string(),
            margin_asset: "USDC".to_string(),
            contract_type: "PERPETUAL".to_string(),
            multi_assets_mode: true,
            enabled: false,
            margin_pct: Decimal::from(25),
            leverage: Decimal::ONE,
            stop_pct: Decimal::ONE,
            take_profit_pct: Decimal::new(4, 2),
            maker_fee_pct: Decimal::ZERO,
            tick_size: Decimal::new(1, 2),
            step_size: Decimal::new(1, 3),
            min_qty: Decimal::new(1, 3),
            min_notional: Decimal::from(5),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.margin_pct <= Decimal::ZERO || self.margin_pct > Decimal::from(100) {
            return Err("仓位比例必须在 0% 到 100% 之间");
        }
        if self.leverage < Decimal::ONE || self.leverage > Decimal::from(125) {
            return Err("杠杆必须在 1 到 125 之间");
        }
        if self.stop_pct <= Decimal::ZERO
            || self.stop_pct > Decimal::from(5)
            || self.take_profit_pct <= Decimal::ZERO
            || self.take_profit_pct > Decimal::from(5)
        {
            return Err("止盈止损比例必须在 0% 到 5% 之间");
        }
        if self.tick_size <= Decimal::ZERO
            || self.step_size <= Decimal::ZERO
            || self.min_qty <= Decimal::ZERO
            || self.min_notional <= Decimal::ZERO
        {
            return Err("交易对价格或数量精度无效");
        }
        if self.maker_fee_pct < Decimal::ZERO || self.maker_fee_pct > Decimal::ONE {
            return Err("Maker 手续费必须在 0% 到 1% 之间");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderKind {
    Entry,
    TakeProfit,
    StopLimit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Open,
    Triggered,
    Filled,
    Canceled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub kind: OrderKind,
    pub side: Side,
    pub status: OrderStatus,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    pub signal: Option<Signal>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Order {
    fn active(&self) -> bool {
        self.status == OrderStatus::Open || self.status == OrderStatus::Triggered
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Position {
    pub side: Side,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub entry_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub stop_price: Decimal,
    pub opened_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetBalances {
    #[serde(with = "rust_decimal::serde::str")]
    pub usdt: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub usdc: Decimal,
}

impl AssetBalances {
    fn amount(&self, asset: &str) -> Decimal {
        match asset {
            "USDT" => self.usdt,
            "USDC" => self.usdc,
            _ => Decimal::ZERO,
        }
    }

    fn add(&mut self, asset: &str, amount: Decimal) {
        match asset {
            "USDT" => self.usdt += amount,
            "USDC" => self.usdc += amount,
            _ => {}
        }
    }

    fn total(&self) -> Decimal {
        self.usdt + self.usdc
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stored {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub mode: ExecutionMode,
    pub config: Config,
    pub wallet: AssetBalances,
    pub realized_pnl: AssetBalances,
    pub position: Option<Position>,
    pub orders: Vec<Order>,
    pub used_signal_at: DateTime<Utc>,
    pub next_order_id: u64,
}

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

impl Stored {
    pub fn initial(symbol: String, now: DateTime<Utc>) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            mode: ExecutionMode::Paper,
            config: Config::initial(symbol),
            wallet: AssetBalances {
                usdt: Decimal::from(10_000),
                usdc: Decimal::ZERO,
            },
            realized_pnl: AssetBalances {
                usdt: Decimal::ZERO,
                usdc: Decimal::ZERO,
            },
            position: None,
            orders: Vec::new(),
            used_signal_at: now,
            next_order_id: 1,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub mode: ExecutionMode,
    pub config: Config,
    pub wallet: AssetBalances,
    #[serde(with = "rust_decimal::serde::str")]
    pub available_collateral: Decimal,
    pub realized_pnl: AssetBalances,
    pub position: Option<Position>,
    pub orders: Vec<Order>,
    pub candle: Option<Candle>,
    pub feed_connected: bool,
    pub feed_fresh: bool,
    pub status: String,
}

pub struct PaperEngine {
    pub stored: Stored,
    pub history: BTreeMap<DateTime<Utc>, Candle>,
    pub live: Option<Candle>,
    pub received_at: Option<DateTime<Utc>>,
    pub feed_connected: bool,
    pub status: String,
}

impl PaperEngine {
    pub fn new(stored: Stored) -> Self {
        Self {
            stored,
            history: BTreeMap::new(),
            live: None,
            received_at: None,
            feed_connected: false,
            status: "等待行情连接".to_string(),
        }
    }

    pub fn snapshot(&self, now: DateTime<Utc>) -> Snapshot {
        Snapshot {
            schema_version: self.stored.schema_version,
            mode: self.stored.mode,
            config: self.stored.config.clone(),
            wallet: self.stored.wallet.clone(),
            available_collateral: self.available_collateral(),
            realized_pnl: self.stored.realized_pnl.clone(),
            position: self.stored.position.clone(),
            orders: self.stored.orders.iter().rev().take(50).cloned().collect(),
            candle: self.live.clone(),
            feed_connected: self.feed_connected,
            feed_fresh: self.fresh(now),
            status: self.status.clone(),
        }
    }

    fn available_collateral(&self) -> Decimal {
        let gross = if self.stored.config.multi_assets_mode {
            // Paper-only parity assumption; live collateral valuation must come from the account API.
            self.stored.wallet.total()
        } else {
            self.stored.wallet.amount(&self.stored.config.margin_asset)
        };
        let reserved = self.stored.position.as_ref().map_or(Decimal::ZERO, |p| {
            p.quantity * p.entry_price / self.stored.config.leverage
        });
        (gross - reserved).max(Decimal::ZERO)
    }

    pub(crate) fn available_collateral_for_backtest(&self) -> Decimal {
        self.available_collateral()
    }

    pub fn fresh(&self, now: DateTime<Utc>) -> bool {
        self.feed_connected
            && self
                .received_at
                .is_some_and(|at| now - at <= Duration::seconds(15))
            && self
                .live
                .as_ref()
                .is_some_and(|c| c.open_time <= now && now < c.open_time + Duration::seconds(75))
    }

    pub fn seed_history(&mut self, candles: Vec<Candle>) {
        for candle in candles {
            if candle.closed {
                self.history.insert(candle.open_time, candle);
            }
        }
        self.prune_history();
    }

    pub fn on_candle(&mut self, candle: Candle, now: DateTime<Utc>) -> bool {
        if candle.closed {
            self.history.insert(candle.open_time, candle.clone());
            self.prune_history();
        }
        self.live = Some(candle);
        self.received_at = Some(now);
        self.feed_connected = true;
        self.drive(now)
    }

    pub fn set_connected(&mut self, connected: bool, now: DateTime<Utc>) -> bool {
        if connected && !self.feed_connected {
            self.live = None;
            self.received_at = None;
        }
        self.feed_connected = connected;
        self.drive(now)
    }

    pub fn update_config(
        &mut self,
        config: Config,
        now: DateTime<Utc>,
    ) -> Result<bool, &'static str> {
        config.validate()?;
        if config.symbol != self.stored.config.symbol
            || config.quote_asset != self.stored.config.quote_asset
            || config.margin_asset != self.stored.config.margin_asset
            || config.contract_type != self.stored.config.contract_type
            || config.tick_size != self.stored.config.tick_size
            || config.step_size != self.stored.config.step_size
            || config.min_qty != self.stored.config.min_qty
            || config.min_notional != self.stored.config.min_notional
        {
            return Err("当前版本运行中不可修改交易对及交易精度");
        }
        self.stored.config = config;
        self.drive(now);
        Ok(true)
    }

    pub fn kill(&mut self, now: DateTime<Utc>) -> bool {
        self.stored.config.enabled = false;
        let mut changed = true;
        changed |= self.cancel_entries(now);
        self.status = "已停用并撤销开仓单；已有保护单继续模拟执行".to_string();
        changed
    }

    pub fn drive(&mut self, now: DateTime<Utc>) -> bool {
        let mut changed = false;
        if !self.fresh(now) {
            changed |= self.cancel_entries(now);
            self.status = "行情断开或超过 15 秒未更新，暂停开仓".to_string();
            return changed;
        }
        let Some(candle) = self.live.clone() else {
            return changed;
        };
        if let Some(index) = self
            .stored
            .orders
            .iter()
            .position(|o| matches!(o.kind, OrderKind::Entry) && o.active())
        {
            let signal = self.stored.orders[index].signal.clone();
            if signal.as_ref().is_none_or(|s| {
                invalid_reason(s, Some(&candle), &self.history, now).is_some()
                    || risk_reason(
                        s,
                        self.stored.config.stop_pct,
                        self.stored.config.take_profit_pct,
                    )
                    .is_some()
            }) || !self.stored.config.enabled
            {
                changed |= self.cancel_entries(now);
                self.status = "回踩信号失效，已撤销开仓单".to_string();
            } else {
                changed |= self.try_fill_entry(index, candle.close, now);
                if self.stored.position.is_none() {
                    self.status = "等待回踩成交".to_string();
                    return changed;
                }
            }
        }
        if self.stored.position.is_some() {
            changed |= self.manage_position(candle.close, now);
            return changed;
        }
        if !self.stored.config.enabled {
            self.status = "策略已关闭".to_string();
            return changed;
        }
        let Some(signal) = find_reversal_signal(&self.history, now, self.stored.config.tick_size)
        else {
            self.status = "等待突破失败及一分钟收盘确认".to_string();
            return changed;
        };
        if signal.confirmed_at <= self.stored.used_signal_at {
            self.status = "等待新信号，不重复使用旧信号".to_string();
            return changed;
        }
        self.stored.used_signal_at = signal.confirmed_at;
        changed = true;
        if let Some(reason) =
            invalid_reason(&signal, Some(&candle), &self.history, now).or_else(|| {
                risk_reason(
                    &signal,
                    self.stored.config.stop_pct,
                    self.stored.config.take_profit_pct,
                )
            })
        {
            self.status = reason.to_string();
            return changed;
        }
        if !stop_before_liquidation(&signal, self.stored.config.leverage) {
            self.status = "信号止损超出杠杆安全范围".to_string();
            return changed;
        }
        if match signal.side {
            Side::Buy => signal.entry_price >= candle.close,
            Side::Sell => signal.entry_price <= candle.close,
        } {
            self.status = "回踩价已穿越当前价，跳过信号".to_string();
            return changed;
        }
        let config = &self.stored.config;
        let quantity = quantize_down(
            self.available_collateral() * config.margin_pct * config.leverage
                / Decimal::from(100)
                / signal.entry_price,
            config.step_size,
        );
        if quantity < config.min_qty || quantity * signal.entry_price < config.min_notional {
            self.status = "可用资金或数量不足，跳过信号".to_string();
            return changed;
        }
        let side = signal.side;
        let price = signal.entry_price;
        let id = signal.id();
        let intent = MakerOrder {
            client_order_id: id.clone(),
            purpose: OrderPurpose::Entry,
            side,
            quantity,
            price,
        };
        if let Err(error) = intent.validate(
            config.tick_size,
            config.step_size,
            config.min_qty,
            config.min_notional,
        ) {
            self.status = format!("Maker 开仓订单校验失败：{error}");
            return changed;
        }
        self.stored.orders.push(Order {
            id,
            kind: OrderKind::Entry,
            side,
            status: OrderStatus::Open,
            quantity,
            price,
            signal: Some(signal),
            created_at: now,
            updated_at: now,
        });
        self.status = format!("等待回踩{} {}", side.label(), price);
        true
    }

    fn try_fill_entry(&mut self, index: usize, last: Decimal, now: DateTime<Utc>) -> bool {
        let order = &mut self.stored.orders[index];
        let reached = match order.side {
            Side::Buy => last <= order.price,
            Side::Sell => last >= order.price,
        };
        if !reached {
            return false;
        }
        let signal = order.signal.as_ref().expect("entry has signal");
        let position = Position {
            side: order.side,
            quantity: order.quantity,
            entry_price: order.price,
            stop_price: signal.stop_price,
            opened_at: now,
        };
        order.status = OrderStatus::Filled;
        order.updated_at = now;
        let fee = position.quantity * position.entry_price * self.stored.config.maker_fee_pct
            / Decimal::from(100);
        let asset = self.stored.config.margin_asset.clone();
        self.stored.wallet.add(&asset, -fee);
        self.stored.realized_pnl.add(&asset, -fee);
        self.stored.position = Some(position);
        self.add_protection(now);
        self.status = "已成交，止盈和限价止损已挂单".to_string();
        true
    }

    fn add_protection(&mut self, now: DateTime<Utc>) {
        let position = self
            .stored
            .position
            .as_ref()
            .expect("position exists")
            .clone();
        let config = &self.stored.config;
        let ratio = config.take_profit_pct / Decimal::from(100);
        let target = match position.side {
            Side::Buy => quantize_up(
                position.entry_price * (Decimal::ONE + ratio),
                config.tick_size,
            ),
            Side::Sell => quantize_down(
                position.entry_price * (Decimal::ONE - ratio),
                config.tick_size,
            ),
        };
        let exit_side = match position.side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let protection = [
            (OrderKind::StopLimit, position.stop_price),
            (OrderKind::TakeProfit, target),
        ];
        for &(kind, price) in &protection {
            let purpose = match kind {
                OrderKind::StopLimit => OrderPurpose::StopLoss,
                OrderKind::TakeProfit => OrderPurpose::TakeProfit,
                OrderKind::Entry => unreachable!("entry is not a protection order"),
            };
            let intent = MakerOrder {
                client_order_id: format!("mm-exit:{}", self.stored.next_order_id),
                purpose,
                side: exit_side,
                quantity: position.quantity,
                price,
            };
            if let Err(error) = intent.validate(
                config.tick_size,
                config.step_size,
                config.min_qty,
                config.min_notional,
            ) {
                self.stored.config.enabled = false;
                self.status = format!("保护单 Maker 校验失败，已停用策略：{error}");
                return;
            }
        }
        for &(kind, price) in &protection {
            let id = format!("mm-exit:{}", self.stored.next_order_id);
            self.stored.next_order_id += 1;
            self.stored.orders.push(Order {
                id,
                kind,
                side: exit_side,
                status: OrderStatus::Open,
                quantity: position.quantity,
                price,
                signal: None,
                created_at: now,
                updated_at: now,
            });
        }
    }

    fn manage_position(&mut self, last: Decimal, now: DateTime<Utc>) -> bool {
        let Some(position) = self.stored.position.clone() else {
            return false;
        };
        let stop = self
            .stored
            .orders
            .iter()
            .position(|o| matches!(o.kind, OrderKind::StopLimit) && o.active());
        let target = self
            .stored
            .orders
            .iter()
            .position(|o| matches!(o.kind, OrderKind::TakeProfit) && o.active());
        if stop.is_none() || target.is_none() {
            self.status = "保护单缺失，暂停并等待人工检查".to_string();
            self.stored.config.enabled = false;
            return true;
        }
        let stop_index = stop.expect("checked");
        let target_index = target.expect("checked");
        let stop_price = self.stored.orders[stop_index].price;
        let trigger = match position.side {
            Side::Buy => last <= stop_price,
            Side::Sell => last >= stop_price,
        };
        let mut changed = false;
        if trigger && self.stored.orders[stop_index].status == OrderStatus::Open {
            self.stored.orders[stop_index].status = OrderStatus::Triggered;
            self.stored.orders[stop_index].updated_at = now;
            changed = true;
        }
        let stop_fill = self.stored.orders[stop_index].status == OrderStatus::Triggered
            && match position.side {
                Side::Buy => last >= stop_price,
                Side::Sell => last <= stop_price,
            };
        let target_price = self.stored.orders[target_index].price;
        let target_fill = match position.side {
            Side::Buy => last >= target_price,
            Side::Sell => last <= target_price,
        };
        let fill_index = if stop_fill {
            Some(stop_index)
        } else if target_fill {
            Some(target_index)
        } else {
            None
        };
        if let Some(index) = fill_index {
            let exit_price = self.stored.orders[index].price;
            self.stored.orders[index].status = OrderStatus::Filled;
            self.stored.orders[index].updated_at = now;
            let other = if index == stop_index {
                target_index
            } else {
                stop_index
            };
            self.stored.orders[other].status = OrderStatus::Canceled;
            self.stored.orders[other].updated_at = now;
            let pnl = match position.side {
                Side::Buy => (exit_price - position.entry_price) * position.quantity,
                Side::Sell => (position.entry_price - exit_price) * position.quantity,
            };
            let fee = exit_price * position.quantity * self.stored.config.maker_fee_pct
                / Decimal::from(100);
            let net = pnl - fee;
            let asset = self.stored.config.margin_asset.clone();
            self.stored.wallet.add(&asset, net);
            self.stored.realized_pnl.add(&asset, net);
            self.stored.position = None;
            self.stored.used_signal_at = now;
            self.status = "已平仓，等待下一次信号".to_string();
            return true;
        }
        self.status = if self.stored.orders[stop_index].status == OrderStatus::Triggered {
            "限价止损已触发，等待价格回到限价".to_string()
        } else {
            "持仓中，止盈与限价止损挂单中".to_string()
        };
        changed
    }

    fn cancel_entries(&mut self, now: DateTime<Utc>) -> bool {
        let mut changed = false;
        for order in &mut self.stored.orders {
            if matches!(order.kind, OrderKind::Entry) && order.active() {
                order.status = OrderStatus::Canceled;
                order.updated_at = now;
                changed = true;
            }
        }
        changed
    }

    fn prune_history(&mut self) {
        while self.history.len() > 120 {
            self.history.pop_first();
        }
    }
}

fn stop_before_liquidation(signal: &Signal, leverage: Decimal) -> bool {
    if leverage <= Decimal::ZERO {
        return false;
    }
    let maintenance = Decimal::new(4, 3);
    let margin = signal.entry_price / leverage;
    match signal.side {
        Side::Buy => {
            let liquidation = (signal.entry_price - margin) / (Decimal::ONE - maintenance);
            signal.stop_price > liquidation.max(Decimal::ZERO)
        }
        Side::Sell => {
            let liquidation = (signal.entry_price + margin) / (Decimal::ONE + maintenance);
            signal.stop_price < liquidation
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn setup() -> (PaperEngine, DateTime<Utc>) {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 8, 6, 0).unwrap();
        let mut stored = Stored::initial("ETHUSDC".to_string(), now - Duration::seconds(1));
        stored.config.enabled = true;
        let mut engine = PaperEngine::new(stored);
        engine.feed_connected = true;
        let start = now - Duration::minutes(66);
        let history = (0..66)
            .map(|i| {
                let open_time = start + Duration::minutes(i);
                Candle {
                    open_time,
                    open: Decimal::new(9980, 2),
                    high: if i == 65 {
                        Decimal::new(10005, 2)
                    } else {
                        Decimal::from(100)
                    },
                    low: Decimal::new(9940, 2),
                    close: if i == 65 {
                        Decimal::new(9998, 2)
                    } else {
                        Decimal::new(9980, 2)
                    },
                    closed: true,
                }
            })
            .collect();
        engine.seed_history(history);
        (engine, now)
    }

    fn live(now: DateTime<Utc>, last: Decimal) -> Candle {
        Candle {
            open_time: now,
            open: last,
            high: last,
            low: last,
            close: last,
            closed: false,
        }
    }

    #[test]
    fn usdt_collateral_opens_usdc_contract_and_pnl_settles_in_usdc() {
        let (mut engine, now) = setup();
        engine.on_candle(live(now, Decimal::new(9998, 2)), now);
        assert_eq!(engine.stored.orders.len(), 1);
        assert_eq!(engine.stored.orders[0].status, OrderStatus::Open);
        engine.on_candle(live(now, Decimal::from(100)), now);
        assert_eq!(engine.stored.orders[0].status, OrderStatus::Filled);
        assert_eq!(engine.stored.orders.len(), 3);
        assert!(engine.stored.position.is_some());
        engine.on_candle(live(now, Decimal::new(9996, 2)), now);
        assert!(engine.stored.position.is_none());
        assert_eq!(engine.stored.wallet.usdt, Decimal::from(10_000));
        assert!(engine.stored.wallet.usdc > Decimal::ZERO);
    }

    #[test]
    fn restart_cancels_unconfirmed_entry_and_does_not_replay_signal() {
        let (mut engine, now) = setup();
        engine.on_candle(live(now, Decimal::new(9998, 2)), now);
        let saved = serde_json::to_vec(&engine.stored).unwrap();
        let mut restarted = PaperEngine::new(serde_json::from_slice(&saved).unwrap());
        assert!(restarted.drive(now + Duration::seconds(1)));
        assert_eq!(restarted.stored.orders[0].status, OrderStatus::Canceled);
        restarted.seed_history(engine.history.values().cloned().collect());
        restarted.on_candle(live(now, Decimal::new(9998, 2)), now);
        assert_eq!(restarted.stored.orders.len(), 1);
    }

    #[test]
    fn single_asset_mode_does_not_use_usdt_for_usdc_contract() {
        let (mut engine, now) = setup();
        engine.stored.config.multi_assets_mode = false;
        engine.on_candle(live(now, Decimal::new(9998, 2)), now);
        assert!(engine.stored.orders.is_empty());
        assert_eq!(engine.snapshot(now).available_collateral, Decimal::ZERO);
    }

    #[test]
    fn fee_is_charged_to_settlement_asset() {
        let (mut engine, now) = setup();
        engine.stored.config.contract_type = "TRADIFI_PERPETUAL".to_string();
        engine.stored.config.margin_asset = "USDT".to_string();
        engine.stored.config.quote_asset = "USDT".to_string();
        engine.stored.config.maker_fee_pct = Decimal::new(2, 2);
        engine.on_candle(live(now, Decimal::new(9998, 2)), now);
        engine.on_candle(live(now, Decimal::from(100)), now);
        assert!(engine.stored.wallet.usdt < Decimal::from(10_000));
        assert_eq!(engine.stored.wallet.usdc, Decimal::ZERO);
    }
}
