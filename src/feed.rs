use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, TimeZone, Utc};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;
use std::time::Duration as StdDuration;
use tokio_tungstenite::tungstenite::Message;

use crate::model::Candle;

pub struct ContractInfo {
    pub quote_asset: String,
    pub margin_asset: String,
    pub contract_type: String,
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub min_qty: Decimal,
    pub min_notional: Decimal,
}

#[derive(Clone)]
pub struct BinanceFeed {
    client: reqwest::Client,
    rest_url: String,
    ws_url: String,
    symbol: String,
}

impl BinanceFeed {
    pub fn new(symbol: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(StdDuration::from_secs(10))
                .build()?,
            rest_url: std::env::var("RUST_CRYPTO_REST_URL")
                .unwrap_or_else(|_| "https://fapi.binance.com".to_string()),
            ws_url: std::env::var("RUST_CRYPTO_WS_URL")
                .unwrap_or_else(|_| "wss://fstream.binance.com".to_string()),
            symbol,
        })
    }

    pub async fn contract_info(&self) -> Result<ContractInfo> {
        let url = format!("{}/fapi/v1/exchangeInfo", self.rest_url);
        let body: Value = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let symbol = body["symbols"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["symbol"] == self.symbol))
            .context("交易对不在 USDⓈ-M 合约交易规则中")?;
        if symbol["status"] != "TRADING" {
            bail!("交易对当前不可交易");
        }
        let filters = symbol["filters"].as_array().context("缺少交易精度规则")?;
        let field = |kind: &str, name: &str| -> Result<Decimal> {
            let filter = filters
                .iter()
                .find(|f| f["filterType"] == kind)
                .with_context(|| format!("缺少 {kind} 规则"))?;
            parse_decimal(&filter[name])
        };
        let tick = field("PRICE_FILTER", "tickSize")?;
        let step = field("LOT_SIZE", "stepSize")?;
        let min_qty = field("LOT_SIZE", "minQty")?;
        let min_notional = filters
            .iter()
            .find(|f| f["filterType"] == "MIN_NOTIONAL")
            .map(|f| parse_decimal(&f["notional"]))
            .transpose()?
            .unwrap_or(Decimal::from(5));
        let string = |name: &str| -> Result<String> {
            symbol[name]
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("缺少合约字段 {name}"))
        };
        let contract_type = string("contractType")?;
        if contract_type != "PERPETUAL" && contract_type != "TRADIFI_PERPETUAL" {
            bail!("当前仅支持永续合约");
        }
        let quote_asset = string("quoteAsset")?;
        let margin_asset = string("marginAsset")?;
        if !matches!(quote_asset.as_str(), "USDT" | "USDC")
            || !matches!(margin_asset.as_str(), "USDT" | "USDC")
        {
            bail!("当前仅支持 USDT 或 USDC 计价与结算的永续合约");
        }
        Ok(ContractInfo {
            quote_asset,
            margin_asset,
            contract_type,
            tick_size: tick,
            step_size: step,
            min_qty,
            min_notional,
        })
    }

    pub async fn history(&self) -> Result<Vec<Candle>> {
        let url = format!("{}/fapi/v1/klines", self.rest_url);
        let rows: Vec<Vec<Value>> = self
            .client
            .get(url)
            .query(&[
                ("symbol", self.symbol.as_str()),
                ("interval", "1m"),
                ("limit", "120"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let now = Utc::now().timestamp_millis();
        rows.iter()
            .filter_map(|row| {
                let open_ms = row.first()?.as_i64()?;
                (open_ms + 60_000 <= now).then_some(row)
            })
            .map(|row| parse_rest_candle(row, now))
            .collect()
    }

    pub async fn latest(&self) -> Result<Candle> {
        let url = format!("{}/fapi/v1/klines", self.rest_url);
        let rows: Vec<Vec<Value>> = self
            .client
            .get(url)
            .query(&[
                ("symbol", self.symbol.as_str()),
                ("interval", "1m"),
                ("limit", "1"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let row = rows.first().context("REST 没有返回最新 K 线")?;
        parse_rest_candle(row, Utc::now().timestamp_millis())
    }

    pub async fn connect(
        &self,
    ) -> Result<impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>> {
        let url = format!("{}/ws/{}@kline_1m", self.ws_url, self.symbol.to_lowercase());
        let (socket, _) = tokio_tungstenite::connect_async(url).await?;
        Ok(socket)
    }
}

pub fn parse_ws_candle(text: &str, symbol: &str) -> Result<Option<Candle>> {
    let value: Value = serde_json::from_str(text)?;
    if value["e"] != "kline" || value["s"] != symbol {
        return Ok(None);
    }
    let k = &value["k"];
    if k["i"] != "1m" {
        return Ok(None);
    }
    Ok(Some(Candle {
        open_time: timestamp(k["t"].as_i64().context("WS K 线时间无效")?)?,
        open: parse_decimal(&k["o"])?,
        high: parse_decimal(&k["h"])?,
        low: parse_decimal(&k["l"])?,
        close: parse_decimal(&k["c"])?,
        closed: k["x"].as_bool().context("WS K 线收盘标志无效")?,
    }))
}

fn parse_decimal(value: &Value) -> Result<Decimal> {
    Decimal::from_str(
        value
            .as_str()
            .ok_or_else(|| anyhow!("交易所数值不是字符串"))?,
    )
    .context("交易所数值无效")
}

fn parse_rest_candle(row: &[Value], now_ms: i64) -> Result<Candle> {
    let open_ms = row
        .first()
        .and_then(Value::as_i64)
        .context("K 线时间无效")?;
    let close_ms = row
        .get(6)
        .and_then(Value::as_i64)
        .context("K 线结束时间无效")?;
    Ok(Candle {
        open_time: timestamp(open_ms)?,
        open: parse_decimal(&row[1])?,
        high: parse_decimal(&row[2])?,
        low: parse_decimal(&row[3])?,
        close: parse_decimal(&row[4])?,
        closed: close_ms <= now_ms,
    })
}

fn timestamp(ms: i64) -> Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .context("交易所时间无效")
}
