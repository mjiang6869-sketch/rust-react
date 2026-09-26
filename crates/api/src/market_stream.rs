//! 盘口与最新价的浏览器推送：`GET /api/v1/market/stream?symbol=ETHUSDC`。
//!
//! # 为什么不复用 `/api/v1/ws`
//!
//! 那条连接推的是引擎状态，与交易对无关，一个页面一条；行情推送按交易对
//! 订阅，切换交易对要换订阅。两者的失败模式也不同——行情上游断了不该让持仓
//! 显示不出来。拆成两条连接，各自重连、各自报错。
//!
//! # 节流
//!
//! 上游盘口每 250ms 一帧，活跃交易对的成交每秒几十笔。每变一次就推一次会让
//! 浏览器每秒重渲染几十次。这里最多每 [`PUSH_INTERVAL`] 推一次**最新的完整
//! 视图**（盘口 + 最新价），中间的变化合并掉——看盘只关心最新值。
//!
//! # 开屏数据
//!
//! 不用 REST 补底：最新价只来自推送里的成交流。连上之前没有任何成交时，
//! `last_price` 为 `null`；第一笔成交到达后才有值。冷门交易对可能要等一会
//! 才看到第一个价——这是刻意的：REST 成交接口权重 20，是全项目最贵的公开
//! 接口，为了「开屏立刻有个数」而打这条 REST 不值得。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{
    Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use exchange::{Interval, LinkState, MarketStreams, MarketView, StreamKind};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

use crate::dto::{ApiError, BookDto, CandleDto, validate_symbol};
use crate::state::AppState;

/// 向浏览器推送的最短间隔。
const PUSH_INTERVAL: Duration = Duration::from_millis(250);

/// 数据来源。界面必须显示。
pub const STREAM_SOURCE: &str = "币安行情推送（非本地归档）";

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    pub symbol: Option<String>,
    pub interval: Option<String>,
}

/// 客户端 → 服务端。只有心跳。
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientMessage {
    Ping,
}

/// 服务端 → 客户端。
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    /// 最新的完整行情视图。装箱是因为它比 `Pong` 大几百字节，而这个枚举
    /// 每帧都要构造一次。
    Market(Box<MarketFrame>),
    Pong,
}

/// 一帧行情。每帧都是完整视图，浏览器直接替换，不做合并。
#[derive(Debug, Serialize)]
struct MarketFrame {
    symbol: String,
    source: &'static str,
    /// 盘口与成交两条上游都在收数据。
    live: bool,
    /// 只是还在建立连接（没有断线、没有限流）。界面据此区分"刚打开页面"
    /// 与"断了"——前者不该弹出红色告警。
    connecting: bool,
    /// 不在线时的原因（中文）。`None` 表示一切正常。
    notice: Option<String>,
    /// 上游限流的剩余冷却毫秒数。0 表示没有冷却。
    cooldown_ms: u64,
    book: Option<BookDto>,
    /// 最新一笔成交的价格。连上之后还没有任何成交时为 `null`——不用 REST
    /// 补底（见模块文档「开屏数据」）。
    #[serde(with = "rust_decimal::serde::str_option")]
    last_price: Option<rust_decimal::Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kline: Option<KlineFrame>,
}

#[derive(Debug, Serialize)]
struct KlineFrame {
    interval: &'static str,
    live: bool,
    notice: Option<String>,
    generation: String,
    candles: Vec<StreamCandleDto>,
}

#[derive(Debug, Serialize)]
struct StreamCandleDto {
    #[serde(flatten)]
    candle: CandleDto,
    event_ms: String,
}

impl KlineFrame {
    fn from_view(interval: Interval, view: &MarketView) -> Self {
        Self {
            interval: interval.as_str(),
            live: view.kline_link == LinkState::Live,
            notice: view.kline_link.describe(StreamKind::Kline(interval)),
            generation: view.kline_generation.to_string(),
            candles: view
                .candles
                .iter()
                .map(|update| StreamCandleDto {
                    candle: CandleDto::from_candle(&update.candle),
                    event_ms: update.event_ms.to_string(),
                })
                .collect(),
        }
    }
}

impl MarketFrame {
    fn from_view(symbol: &str, v: &MarketView, cooldown_ms: u64) -> Self {
        let notices: Vec<String> = [StreamKind::Depth, StreamKind::Trades]
            .into_iter()
            .filter_map(|k| v.link(k).describe(k))
            .collect();
        Self {
            symbol: symbol.to_string(),
            source: STREAM_SOURCE,
            live: v.is_live(),
            connecting: !v.is_live()
                && [StreamKind::Depth, StreamKind::Trades]
                    .into_iter()
                    .all(|k| matches!(v.link(k), LinkState::Live | LinkState::Connecting)),
            notice: (!notices.is_empty()).then(|| notices.join("；")),
            cooldown_ms,
            book: v.book.clone().map(|b| BookDto::from_snapshot(symbol, b)),
            last_price: v.trades.front().map(|t| t.price.get()),
            kline: None,
        }
    }
}

/// 升级为 WebSocket。
///
/// 交易对在升级**之前**校验：它会被拼进上游 URL，而且校验失败应当是一个
/// 普通的 400，而不是一条连上就断的 WebSocket。
pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StreamQuery>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let symbol = match validate_symbol(q.symbol.as_deref().unwrap_or(&state.symbol())) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let interval = match q.interval.as_deref().map(Interval::parse) {
        Some(Some(interval)) => Some(interval),
        None => None,
        Some(None) => return ApiError::BadRequest("不支持的 K 线周期".into()).into_response(),
    };
    let Some(streams) = state.market_streams().cloned() else {
        return ApiError::Internal(
            "行情推送未启用：行情地址被指向了非生产网，推送只接币安生产网".into(),
        )
        .into_response();
    };
    match ws {
        Ok(ws) => ws
            .on_upgrade(move |socket| serve(socket, state, streams, symbol, interval))
            .into_response(),
        Err(rejection) => rejection.into_response(),
    }
}

async fn serve(
    socket: WebSocket,
    state: Arc<AppState>,
    streams: MarketStreams,
    symbol: String,
    interval: Option<Interval>,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = streams.subscribe(&symbol);
    let mut kline_rx = interval.map(|iv| streams.subscribe_klines(&symbol, iv));

    let mut tick = tokio::time::interval(PUSH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 连上立刻推一帧：即便上游还在连接中，界面也要知道"正在连"，而不是空白。
    let mut dirty = true;
    let mut upstream_gone = false;

    loop {
        tokio::select! {
            changed = rx.changed(), if !dirty && !upstream_gone => {
                // 发送端只会在无人订阅时关闭；我们自己就是订阅者，走到这里
                // 说明订阅表出了问题。停止等待变化，但保持连接，让浏览器看到
                // 最后一帧里的状态说明。
                if changed.is_err() {
                    upstream_gone = true;
                    tracing::warn!(symbol, "行情推送的上游通道已关闭");
                }
                dirty = true;
            }
            changed = async {
                match kline_rx.as_mut() {
                    Some(rx) => rx.changed().await,
                    None => std::future::pending().await,
                }
            }, if !dirty => {
                if changed.is_err() { break; }
                dirty = true;
            }
            _ = tick.tick(), if dirty => {
                dirty = false;
                let cooldown_ms = state.market_cooldown().remaining_ms().unwrap_or(0);
                let mut frame = MarketFrame::from_view(&symbol, &rx.borrow_and_update(), cooldown_ms);
                if let (Some(iv), Some(rx)) = (interval, kline_rx.as_mut()) {
                    frame.kline = Some(KlineFrame::from_view(iv, &rx.borrow_and_update()));
                }
                if send(&mut sender, &ServerMessage::Market(Box::new(frame))).await.is_err() {
                    break;
                }
            }
            msg = receiver.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ClientMessage::Ping) = serde_json::from_str(&text)
                        && send(&mut sender, &ServerMessage::Pong).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::debug!(symbol, "行情推送连接错误：{e}");
                    break;
                }
            },
        }
    }
    // `rx` 在这里被丢弃；它是最后一个订阅者时，上游在宽限期后关闭。
}

async fn send(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &ServerMessage,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).unwrap_or_default();
    sender.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use domain::{AggTrade, BookSnapshot, Price, Qty};
    use rust_decimal_macros::dec;

    fn view() -> MarketView {
        let mut v = MarketView {
            book: Some(BookSnapshot {
                bid: dec!(100),
                ask: dec!(101),
                bids: vec![(dec!(100), dec!(1))],
                asks: vec![(dec!(101), dec!(2))],
                at: Utc::now(),
            }),
            depth_link: LinkState::Live,
            trades_link: LinkState::Live,
            ..MarketView::default()
        };
        v.push_trade(AggTrade {
            trade_id: 7,
            price: Price::new(dec!(100.5)),
            quantity: Qty::new(dec!(0.25)),
            is_buyer_maker: true,
            at: Utc::now(),
        });
        v
    }

    /// 帧格式是前端的契约：带 `type` 标签、数值是字符串、必带来源。
    #[test]
    fn frame_serializes_with_source_and_string_decimals() {
        let frame = MarketFrame::from_view("ETHUSDC", &view(), 0);
        let j = serde_json::to_value(ServerMessage::Market(Box::new(frame))).unwrap();
        assert_eq!(j["type"], "market");
        assert_eq!(j["source"], STREAM_SOURCE);
        assert_eq!(j["live"], true);
        assert!(j["notice"].is_null());
        // 与 REST 的 `BookDto` 同一套序列化：保留小数位，(100 + 101) / 2 = "100.50"
        assert_eq!(j["book"]["mid"], "100.50");
        assert_eq!(j["last_price"], "100.5");
    }

    /// 上游不在线时，帧里必须带着原因——界面要说清为什么不是实时的。
    #[test]
    fn frame_explains_why_it_is_not_live() {
        let mut v = view();
        v.trades_link = LinkState::Retrying {
            attempt: 2,
            retry_in_ms: 1_500,
            reason: "上游关闭了连接".into(),
        };
        let frame = MarketFrame::from_view("ETHUSDC", &v, 0);
        assert!(!frame.live);
        assert!(!frame.connecting, "断线不是'连接中'，界面要告警");
        let notice = frame.notice.expect("应有说明");
        assert!(notice.contains("成交流"), "{notice}");
        assert!(notice.contains("上游关闭了连接"), "{notice}");
        assert!(notice.contains("2 秒"), "1.5 秒向上取整：{notice}");
    }

    /// 刚打开页面、上游还在握手：不在线，但也不是故障。
    #[test]
    fn fresh_subscription_is_connecting_not_broken() {
        let frame = MarketFrame::from_view("ETHUSDC", &MarketView::default(), 0);
        assert!(!frame.live);
        assert!(frame.connecting);
    }

    /// 还没有任何成交到达时，最新价必须是 `null`——不用 REST 补一个假值。
    #[test]
    fn last_price_is_null_before_first_trade() {
        let frame = MarketFrame::from_view("ETHUSDC", &MarketView::default(), 0);
        let j = serde_json::to_value(ServerMessage::Market(Box::new(frame))).unwrap();
        assert!(j["last_price"].is_null());
    }

    #[test]
    fn kline_frame_uses_seconds_and_string_event_time_and_decimals() {
        let mut view = MarketView {
            kline_link: LinkState::Live,
            kline_generation: 2,
            ..MarketView::default()
        };
        view.push_candle(exchange::stream::StreamCandle {
            candle: domain::Candle {
                open_time: chrono::DateTime::from_timestamp(1_790_353_800, 0).unwrap(),
                open: dec!(100),
                high: dec!(103),
                low: dec!(99),
                close: dec!(102.25),
                volume: dec!(12.3),
                closed: true,
            },
            event_ms: 1_790_354_700_001,
        });
        let json = serde_json::to_value(KlineFrame::from_view(Interval::M15, &view)).unwrap();
        assert_eq!(json["interval"], "15m");
        assert_eq!(json["generation"], "2");
        assert_eq!(json["live"], true);
        assert_eq!(json["candles"][0]["time"], 1_790_353_800);
        assert_eq!(json["candles"][0]["event_ms"], "1790354700001");
        assert_eq!(json["candles"][0]["close"], "102.25");
        assert_eq!(json["candles"][0]["volume"], "12.3");
        assert_eq!(json["candles"][0]["closed"], true);
    }

    #[test]
    fn ping_parses() {
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"op":"ping"}"#),
            Ok(ClientMessage::Ping)
        ));
    }
}
