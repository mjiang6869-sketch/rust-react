//! 币安行情 WebSocket：盘口（partial depth）与聚合成交（aggTrade）。
//!
//! # 为什么要有这个模块（事故复盘）
//!
//! 界面原先用 REST 每秒轮询盘口与成交流。`/fapi/v1/aggTrades` 的权重是 20，
//! 光这一项每分钟就是 1200 权重；再加上盘口与 K 线，一个标签页约 1380 权重/
//! 分钟。重复轮询把它翻倍到约 2760，越过 2400 的按 IP 上限，先 429，继续打就
//! 升级成 418 封禁。
//!
//! 推送流**不按请求计权重**。盘口与成交流改走这里之后，界面上最贵的两项 REST
//! 轮询消失；K 线也使用原生推送，REST 只用于历史初始化与断线补齐。
//!
//! # 路由路径
//!
//! 币安 USDⓈ-M 的行情推送按流的类别拆成了带路径的入口：
//!
//! | 路径 | 本模块用到的流 |
//! |---|---|
//! | `/public` | `<symbol>@depth<N>`（盘口，默认 250ms 一帧） |
//! | `/market` | `<symbol>@aggTrade`（聚合成交） |
//!
//! 不带路径的旧入口只推 `/public` 那一类，**连得上但收不到成交**——这种"静默
//! 无数据"最难发现。所以每条连接都有空闲超时：连上后规定时间内一条数据都没有，
//! 视为故障并重连，同时在界面上显示原因。
//!
//! # 一个交易对一份上游，与浏览器数量无关
//!
//! 和 REST 缓存同一个思路：上游连接数只由"有几个交易对在被看"决定，不由"开了
//! 几个标签页"决定。每个交易对的最新视图放在一个 `watch` 通道里——它只保留
//! 最新值，天然适合"盘口只关心最新一帧"。所有订阅者都走开之后，上游连接在一
//! 小段宽限期后关闭（React 开发模式会挂载、卸载、再挂载，没有宽限期会让上游
//! 连接随之抖动）。
//!
//! # 重连纪律
//!
//! - 连接超时、空闲超时都有上限，不会卡死在半开连接上。
//! - 断线后指数退避并加抖动；连接稳定一段时间后才把退避复位，避免"连上立刻被
//!   踢、又立刻重连"的快速循环。
//! - 与 REST **共用同一份冷却**：封禁记在 IP 上，REST 撞到 418 期间推送也不该
//!   去握手；握手本身被 429/418 拒绝时同样会点亮这份冷却。
//!
//! # 端点白名单
//!
//! 推送的主机与 REST 的签名白名单**分开维护**（见 [`STREAM_ALLOWED_HOSTS`]）。
//! 签名白名单是"凭据能发往哪里"的安全边界，这里的连接不带任何凭据，但把
//! `fstream.binance.com` 加进签名白名单会无谓地扩大凭据的可达范围。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::market::Interval;
use chrono::{TimeZone, Utc};
use domain::{AggTrade, BookSnapshot, Candle, Price, Qty};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::cooldown::{
    Cooldown, DEFAULT_RETRY_AFTER_MS, parse_banned_until_ms, parse_retry_after_opt,
};
use crate::error::ExchangeError;

/// 生产环境行情推送入口。
pub const PRODUCTION_STREAM_URL: &str = "wss://fstream.binance.com";

/// 允许的推送主机。与签名白名单分开，理由见模块文档。
const STREAM_ALLOWED_HOSTS: &[&str] = &["fstream.binance.com"];

/// 盘口档数。币安 partial depth 只支持 5 / 10 / 20。
pub const DEPTH_LEVELS: u32 = 20;

/// 最近成交保留的笔数。与界面成交流的行数一致（原 REST 轮询也是 30 笔）。
pub const RECENT_TRADES_CAP: usize = 30;

/// 推送验证：主机必须在白名单内，且必须是加密连接。
pub fn stream_endpoint_allowed(url: &str) -> Result<(), ExchangeError> {
    let parsed = url::Url::parse(url)
        .map_err(|_| ExchangeError::Fatal(format!("推送地址无法解析：{url}")))?;
    if parsed.scheme() != "wss" {
        return Err(ExchangeError::Fatal(format!(
            "推送地址必须是 wss://，收到：{url}"
        )));
    }
    let host = parsed.host_str().unwrap_or_default();
    if STREAM_ALLOWED_HOSTS.contains(&host) {
        Ok(())
    } else {
        Err(ExchangeError::Fatal(format!(
            "推送主机不在白名单内：{host}。只允许币安官方行情推送域名。"
        )))
    }
}

// ---------------------------------------------------------------------------
// 流的种类与地址
// ---------------------------------------------------------------------------

/// 一条上游连接承载的数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    /// 盘口（`/public`）。
    Depth,
    /// 聚合成交（`/market`）。
    Trades,
    /// 原生 K 线（按周期共享连接）。
    Kline(Interval),
}

impl StreamKind {
    /// 币安的路由路径。放错路径会连得上但收不到数据。
    pub fn route(self) -> &'static str {
        match self {
            StreamKind::Depth => "public",
            StreamKind::Trades | StreamKind::Kline(_) => "market",
        }
    }

    /// 流名。币安要求交易对小写。
    pub fn stream_name(self, symbol: &str) -> String {
        let s = symbol.to_ascii_lowercase();
        match self {
            // 不带速度后缀 = 默认 250ms，正好是后端向浏览器推送的节奏。
            // 更快的 `@100ms` 只会在后端被合并丢弃。
            StreamKind::Depth => format!("{s}@depth{DEPTH_LEVELS}"),
            StreamKind::Trades => format!("{s}@aggTrade"),
            StreamKind::Kline(interval) => format!("{s}@kline_{}", interval.as_str()),
        }
    }

    /// 给人看的名字。
    pub fn label(self) -> &'static str {
        match self {
            StreamKind::Depth => "盘口",
            StreamKind::Trades => "成交流",
            StreamKind::Kline(_) => "K 线",
        }
    }
}

/// 拼出一条流的完整地址。
///
/// 用组合流入口（`/stream?streams=`）而不是 `/ws/<name>`：组合流的消息带着
/// 流名外壳，日志里出现解析失败时能直接看出是哪条流。
pub fn stream_url(base: &str, kind: StreamKind, symbol: &str) -> String {
    format!(
        "{}/{}/stream?streams={}",
        base.trim_end_matches('/'),
        kind.route(),
        kind.stream_name(symbol)
    )
}

// ---------------------------------------------------------------------------
// 消息解析
// ---------------------------------------------------------------------------

/// 去掉组合流的 `{"stream": ..., "data": ...}` 外壳。直连单流时没有外壳。
fn unwrap_envelope(text: &str) -> Result<serde_json::Value, String> {
    let mut v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("不是合法 JSON：{e}"))?;
    if let Some(data) = v.get_mut("data") {
        return Ok(data.take());
    }
    Ok(v)
}

fn parse_decimal(raw: &str, what: &str) -> Result<Decimal, String> {
    raw.parse::<Decimal>()
        .map_err(|_| format!("{what}无法解析为数值：{raw}"))
}

fn parse_levels(raw: &[[String; 2]]) -> Result<Vec<(Decimal, Decimal)>, String> {
    raw.iter()
        .map(|[p, q]| Ok((parse_decimal(p, "价格")?, parse_decimal(q, "数量")?)))
        .collect()
}

/// 解析一帧 partial depth。
///
/// partial depth 每一帧都是**完整的前 N 档**，不是增量——所以不需要维护本地
/// 订单簿，也不存在"丢一帧就错位"的问题。字段名兼容 `b`/`a` 与 `bids`/`asks`
/// 两种写法（期货与现货不同）。
///
/// 单个数值解析失败时**整帧丢弃**，而不是像 REST 那样跳过坏档：盘口少一档
/// 会让累计深度悄悄失真，宁可沿用上一帧。
pub fn parse_depth_event(text: &str) -> Result<BookSnapshot, String> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(rename = "E", default)]
        event_time: i64,
        #[serde(rename = "b", alias = "bids")]
        bids: Vec<[String; 2]>,
        #[serde(rename = "a", alias = "asks")]
        asks: Vec<[String; 2]>,
    }

    let data = unwrap_envelope(text)?;
    let raw: Raw = serde_json::from_value(data).map_err(|e| format!("盘口字段不符：{e}"))?;
    let bids = parse_levels(&raw.bids)?;
    let asks = parse_levels(&raw.asks)?;
    let bid = bids.first().map(|(p, _)| *p).unwrap_or(Decimal::ZERO);
    let ask = asks.first().map(|(p, _)| *p).unwrap_or(Decimal::ZERO);
    let at = Utc
        .timestamp_millis_opt(raw.event_time)
        .single()
        .filter(|_| raw.event_time > 0)
        .unwrap_or_else(Utc::now);
    Ok(BookSnapshot {
        bid,
        ask,
        bids,
        asks,
        at,
    })
}

/// 解析一笔聚合成交。
pub fn parse_agg_trade(text: &str) -> Result<AggTrade, String> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(rename = "e", default)]
        event: String,
        #[serde(rename = "a")]
        id: u64,
        #[serde(rename = "p")]
        price: String,
        #[serde(rename = "q")]
        qty: String,
        #[serde(rename = "T")]
        time: i64,
        #[serde(rename = "m")]
        is_buyer_maker: bool,
    }

    let data = unwrap_envelope(text)?;
    let raw: Raw = serde_json::from_value(data).map_err(|e| format!("成交字段不符：{e}"))?;
    if !raw.event.is_empty() && raw.event != "aggTrade" {
        return Err(format!("不是聚合成交事件：{}", raw.event));
    }
    let at = Utc
        .timestamp_millis_opt(raw.time)
        .single()
        .ok_or_else(|| format!("成交时间无效：{}", raw.time))?;
    Ok(AggTrade {
        trade_id: raw.id,
        price: Price::new(parse_decimal(&raw.price, "成交价")?),
        quantity: Qty::new(parse_decimal(&raw.qty, "成交量")?),
        is_buyer_maker: raw.is_buyer_maker,
        at,
    })
}

/// 原生 K 线快照；事件时刻用于拒绝乱序更新，价格始终保持 Decimal。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamCandle {
    pub candle: Candle,
    pub event_ms: i64,
}

pub fn parse_kline_event(
    text: &str,
    symbol: &str,
    interval: Interval,
) -> Result<StreamCandle, String> {
    #[derive(Deserialize)]
    struct Raw {
        e: String,
        #[serde(rename = "E")]
        event_ms: i64,
        s: String,
        k: Bar,
    }
    #[derive(Deserialize)]
    struct Bar {
        t: i64,
        s: String,
        i: String,
        o: String,
        h: String,
        l: String,
        c: String,
        v: String,
        x: bool,
    }
    let raw: Raw =
        serde_json::from_value(unwrap_envelope(text)?).map_err(|e| format!("K 线字段不符：{e}"))?;
    if raw.e != "kline" || raw.s != symbol || raw.k.s != symbol || raw.k.i != interval.as_str() {
        return Err("K 线事件的交易对或周期与订阅不符".into());
    }
    let open_time = Utc
        .timestamp_millis_opt(raw.k.t)
        .single()
        .filter(|_| raw.k.t > 0 && raw.event_ms >= raw.k.t)
        .ok_or_else(|| "K 线时间无效".to_string())?;
    Ok(StreamCandle {
        event_ms: raw.event_ms,
        candle: Candle {
            open_time,
            open: parse_decimal(&raw.k.o, "开盘价")?,
            high: parse_decimal(&raw.k.h, "最高价")?,
            low: parse_decimal(&raw.k.l, "最低价")?,
            close: parse_decimal(&raw.k.c, "收盘价")?,
            volume: parse_decimal(&raw.k.v, "成交量")?,
            closed: raw.k.x,
        },
    })
}

// ---------------------------------------------------------------------------
// 视图
// ---------------------------------------------------------------------------

/// 一条上游连接的状态。界面据此说明"为什么不是实时的"。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LinkState {
    /// 正在建立连接，或已连上但还没收到第一条数据。
    #[default]
    Connecting,
    /// 正在收数据。
    Live,
    /// 断开了，等待重连。
    Retrying {
        attempt: u32,
        retry_in_ms: u64,
        reason: String,
    },
    /// 上游限流冷却中，暂不握手。
    CoolingDown { remaining_ms: u64 },
}

/// 某个交易对的最新行情视图。
#[derive(Clone, Debug, Default)]
pub struct MarketView {
    /// 最新一帧盘口。
    pub book: Option<BookSnapshot>,
    /// 最近成交，**新的在前**。
    pub trades: VecDeque<AggTrade>,
    pub depth_link: LinkState,
    pub trades_link: LinkState,
    pub kline_link: LinkState,
    /// 原生快照按开盘时间升序，保留近期收盘帧，避免节流丢掉收盘事件。
    pub candles: VecDeque<StreamCandle>,
    /// 每次上游握手成功递增；浏览器即使没看到短暂断线，也能补历史。
    pub kline_generation: u64,
}

impl LinkState {
    /// 给人看的一句话。`None` 表示正常，不需要提示。
    pub fn describe(&self, kind: StreamKind) -> Option<String> {
        let label = kind.label();
        match self {
            LinkState::Live => None,
            LinkState::Connecting => Some(format!("{label}推送连接中")),
            LinkState::Retrying {
                attempt,
                retry_in_ms,
                reason,
            } => Some(format!(
                "{label}推送断开（{reason}），第 {attempt} 次重连将在约 {} 秒后进行",
                retry_in_ms.div_ceil(1_000)
            )),
            LinkState::CoolingDown { remaining_ms } => Some(format!(
                "币安接口限流，{label}推送暂停约 {} 秒",
                remaining_ms.div_ceil(1_000)
            )),
        }
    }
}

impl MarketView {
    pub fn link(&self, kind: StreamKind) -> &LinkState {
        match kind {
            StreamKind::Depth => &self.depth_link,
            StreamKind::Trades => &self.trades_link,
            StreamKind::Kline(_) => &self.kline_link,
        }
    }

    fn link_mut(&mut self, kind: StreamKind) -> &mut LinkState {
        match kind {
            StreamKind::Depth => &mut self.depth_link,
            StreamKind::Trades => &mut self.trades_link,
            StreamKind::Kline(_) => &mut self.kline_link,
        }
    }

    pub fn push_candle(&mut self, update: StreamCandle) -> bool {
        if let Some(previous) = self.candles.back() {
            if update.candle.open_time < previous.candle.open_time {
                return false;
            }
            if update.candle.open_time == previous.candle.open_time {
                if previous.candle.closed
                    || update.event_ms < previous.event_ms
                    || *previous == update
                {
                    return false;
                }
                self.candles.pop_back();
            }
        }
        self.candles.push_back(update);
        while self.candles.len() > 32 {
            self.candles.pop_front();
        }
        true
    }

    /// 两条连接都在收数据。
    pub fn is_live(&self) -> bool {
        self.depth_link == LinkState::Live && self.trades_link == LinkState::Live
    }

    /// 记入一笔成交。重复或更旧的成交（重连后常见）被丢弃。
    ///
    /// 返回是否真的写入了。
    pub fn push_trade(&mut self, t: AggTrade) -> bool {
        if let Some(latest) = self.trades.front()
            && t.trade_id <= latest.trade_id
        {
            return false;
        }
        self.trades.push_front(t);
        self.trades.truncate(RECENT_TRADES_CAP);
        true
    }

    /// 并入一批成交（REST 补的开屏数据）。按成交 ID 去重、新的在前。
    ///
    /// 与 [`Self::push_trade`] 分开：推送来的成交只会更新，补数据来的可能比
    /// 已有的旧，需要插到中间而不是被丢弃。
    pub fn merge_trades(&mut self, batch: impl IntoIterator<Item = AggTrade>) {
        let mut all: Vec<AggTrade> = self.trades.drain(..).chain(batch).collect();
        all.sort_by_key(|t| std::cmp::Reverse(t.trade_id));
        all.dedup_by_key(|t| t.trade_id);
        all.truncate(RECENT_TRADES_CAP);
        self.trades = all.into();
    }
}

// ---------------------------------------------------------------------------
// 退避
// ---------------------------------------------------------------------------

/// 指数退避，带 0–20% 的抖动。
///
/// 抖动取自当前时刻的纳秒部分，不为这点随机性引入随机数依赖。它的作用只是
/// 让多个连接不要在同一瞬间一齐重连，不需要密码学意义上的随机。
#[derive(Debug)]
struct Backoff {
    initial: Duration,
    max: Duration,
    current: Duration,
    attempt: u32,
}

impl Backoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            current: initial,
            attempt: 0,
        }
    }

    /// 下一次等待时长，并把退避推进一级。
    fn next_delay(&mut self) -> Duration {
        let base = self.current;
        self.current = (self.current * 2).min(self.max);
        self.attempt = self.attempt.saturating_add(1);
        let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
        let jitter_ms = base_ms / 1_000 * u64::from(Utc::now().timestamp_subsec_nanos() % 200);
        (base + Duration::from_millis(jitter_ms)).min(self.max)
    }

    fn reset(&mut self) {
        self.current = self.initial;
        self.attempt = 0;
    }
}

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// 连接参数。生产值见 [`StreamConfig::production`]。
#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// 推送入口。
    pub base: String,
    /// 握手超时。
    pub connect_timeout: Duration,
    /// 盘口连接的空闲超时。盘口按固定频率推送，30 秒没有数据一定是出了问题
    /// （包括"路由路径错了、连得上但收不到"）。
    pub depth_idle_timeout: Duration,
    /// 成交连接的空闲超时。冷门交易对可能几分钟没有成交，但币安每 3 分钟会
    /// 发一次 ping，所以超过 4 分钟完全没动静就是断了。
    pub trades_idle_timeout: Duration,
    /// 首次重连等待。
    pub backoff_initial: Duration,
    /// 重连等待上限。
    pub backoff_max: Duration,
    /// 连接存活超过这个时长才把退避复位。
    pub stable_after: Duration,
    /// 最后一个订阅者离开后，上游连接再保留多久。
    pub linger: Duration,
    /// 是否校验推送主机白名单。只有测试会关掉它。
    enforce_whitelist: bool,
}

impl StreamConfig {
    pub fn production() -> Self {
        Self {
            base: PRODUCTION_STREAM_URL.to_string(),
            connect_timeout: Duration::from_secs(10),
            depth_idle_timeout: Duration::from_secs(30),
            trades_idle_timeout: Duration::from_secs(240),
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
            linger: Duration::from_secs(15),
            enforce_whitelist: true,
        }
    }

    fn idle_timeout(&self, kind: StreamKind) -> Duration {
        match kind {
            StreamKind::Depth => self.depth_idle_timeout,
            StreamKind::Trades | StreamKind::Kline(_) => self.trades_idle_timeout,
        }
    }

    /// 本地假上游用的参数：时间全部缩短，不校验白名单。
    #[cfg(test)]
    fn local_test(base: String) -> Self {
        Self {
            base,
            connect_timeout: Duration::from_secs(2),
            depth_idle_timeout: Duration::from_secs(2),
            trades_idle_timeout: Duration::from_secs(2),
            backoff_initial: Duration::from_millis(20),
            backoff_max: Duration::from_millis(200),
            stable_after: Duration::from_secs(60),
            linger: Duration::from_millis(50),
            enforce_whitelist: false,
        }
    }
}

// ---------------------------------------------------------------------------
// 订阅中心
// ---------------------------------------------------------------------------

struct Hub {
    config: StreamConfig,
    cooldown: Cooldown,
    feeds: Mutex<HashMap<String, watch::Sender<MarketView>>>,
}

impl Hub {
    /// 没人订阅了就退役：在锁内复查订阅者数量，并只移除**自己那一份**。
    ///
    /// 在锁内复查是关键：`subscribe` 也在同一把锁内创建订阅者，所以"判断没人
    /// → 移除"与"有人新订阅"不会交错，新订阅者不会拿到一个即将关闭的通道。
    fn retire(&self, symbol: &str, tx: &watch::Sender<MarketView>) -> bool {
        let Ok(mut feeds) = self.feeds.lock() else {
            // 锁中毒时保守处理：退役这条连接。最坏情况是下一个订阅者重新连一次。
            return true;
        };
        if tx.receiver_count() > 0 {
            return false;
        }
        if feeds.get(symbol).is_some_and(|cur| cur.same_channel(tx)) {
            feeds.remove(symbol);
        }
        true
    }
}

/// 行情推送订阅中心。`Clone` 共享同一份状态。
#[derive(Clone)]
pub struct MarketStreams {
    hub: Arc<Hub>,
}

impl MarketStreams {
    /// 生产配置，与 REST 共用冷却。
    ///
    /// 构造时**不连网**：上游连接在第一次 `subscribe` 时才建立。
    pub fn production(cooldown: Cooldown) -> Self {
        Self::with_config(StreamConfig::production(), cooldown)
    }

    pub fn with_config(config: StreamConfig, cooldown: Cooldown) -> Self {
        Self {
            hub: Arc::new(Hub {
                config,
                cooldown,
                feeds: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// 订阅某个交易对。第一次订阅时启动上游连接。
    ///
    /// `symbol` 必须已经校验过（只含字母数字）——它会被拼进上游 URL。
    /// 必须在 tokio 运行时内调用。
    pub fn subscribe(&self, symbol: &str) -> watch::Receiver<MarketView> {
        self.subscribe_kinds(
            symbol,
            symbol.to_ascii_uppercase(),
            &[StreamKind::Depth, StreamKind::Trades],
        )
    }

    pub fn subscribe_klines(
        &self,
        symbol: &str,
        interval: Interval,
    ) -> watch::Receiver<MarketView> {
        let key = format!(
            "{}@kline_{}",
            symbol.to_ascii_uppercase(),
            interval.as_str()
        );
        self.subscribe_kinds(symbol, key, &[StreamKind::Kline(interval)])
    }

    fn subscribe_kinds(
        &self,
        symbol: &str,
        key: String,
        kinds: &[StreamKind],
    ) -> watch::Receiver<MarketView> {
        let symbol = symbol.to_ascii_uppercase();
        let Ok(mut feeds) = self.hub.feeds.lock() else {
            // 锁中毒：返回一个永远处于"连接中"的视图，界面会显示未就绪并走
            // REST 兜底，而不是让整个请求 panic。
            let (_tx, rx) = watch::channel(MarketView::default());
            tracing::error!(symbol, "行情推送订阅表锁中毒");
            return rx;
        };
        if let Some(tx) = feeds.get(&key) {
            return tx.subscribe();
        }
        let (tx, rx) = watch::channel(MarketView::default());
        feeds.insert(key.clone(), tx.clone());
        drop(feeds);

        for &kind in kinds {
            tokio::spawn(run_link(
                self.hub.clone(),
                symbol.clone(),
                key.clone(),
                kind,
                tx.clone(),
            ));
        }
        rx
    }

    /// 用 REST 拉到的成交给视图补底。
    ///
    /// 推送只带**连上之后**的成交；刚打开页面时成交流是空的，冷门交易对可能
    /// 要空很久。补底只在视图里还没有该交易对时生效——没人订阅就不补。
    pub fn seed_trades(&self, symbol: &str, trades: Vec<AggTrade>) {
        let symbol = symbol.to_ascii_uppercase();
        let tx = match self.hub.feeds.lock() {
            Ok(feeds) => feeds.get(&symbol).cloned(),
            Err(_) => None,
        };
        if let Some(tx) = tx {
            tx.send_modify(|v| v.merge_trades(trades));
        }
    }

    /// 当前有上游连接的交易对数量。
    pub fn active_feeds(&self) -> usize {
        self.hub.feeds.lock().map(|f| f.len()).unwrap_or(0)
    }
}

/// 等到没有订阅者、且宽限期过去仍然没有，再返回。
async fn until_unused(hub: &Hub, symbol: &str, tx: &watch::Sender<MarketView>) {
    loop {
        tx.closed().await;
        tokio::time::sleep(hub.config.linger).await;
        if hub.retire(symbol, tx) {
            return;
        }
    }
}

/// 更新某条连接的状态。状态没变就不通知订阅者。
fn set_link(tx: &watch::Sender<MarketView>, kind: StreamKind, state: LinkState) {
    tx.send_if_modified(|v| {
        let slot = v.link_mut(kind);
        if *slot == state {
            return false;
        }
        *slot = state;
        true
    });
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 握手失败的两种结局。
enum ConnectError {
    /// 被限流：等待时间来自响应。
    RateLimited { retry_after_ms: u64, status: u16 },
    /// 其它失败：退避后重试。
    Other(String),
}

async fn connect_once(config: &StreamConfig, url: &str) -> Result<WsStream, ConnectError> {
    if config.enforce_whitelist {
        stream_endpoint_allowed(url).map_err(|e| ConnectError::Other(e.to_string()))?;
    }
    let attempt = tokio_tungstenite::connect_async(url);
    match tokio::time::timeout(config.connect_timeout, attempt).await {
        Err(_) => Err(ConnectError::Other(format!(
            "握手超时（{} 秒）",
            config.connect_timeout.as_secs()
        ))),
        Ok(Ok((ws, _resp))) => Ok(ws),
        Ok(Err(tungstenite::Error::Http(resp))) => {
            let status = resp.status().as_u16();
            if status == 429 || status == 418 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok());
                let body = resp
                    .body()
                    .as_deref()
                    .map(String::from_utf8_lossy)
                    .unwrap_or_default();
                let ms = parse_retry_after_opt(retry_after)
                    .or_else(|| parse_banned_until_ms(&body, Utc::now().timestamp_millis()))
                    .unwrap_or(DEFAULT_RETRY_AFTER_MS);
                Err(ConnectError::RateLimited {
                    retry_after_ms: ms,
                    status,
                })
            } else {
                Err(ConnectError::Other(format!("握手被拒：HTTP {status}")))
            }
        }
        Ok(Err(e)) => Err(ConnectError::Other(format!("连接失败：{e}"))),
    }
}

/// 读一条连接直到它断开。返回断开原因。
async fn pump(
    ws: &mut WsStream,
    kind: StreamKind,
    symbol: &str,
    tx: &watch::Sender<MarketView>,
    idle: Duration,
) -> String {
    // 同一条连接上的解析失败只 warn 一次：格式变了会每 250ms 失败一次，
    // 每次都 warn 会把日志刷满，反而淹没第一条有用的信息。
    let mut parse_warned = false;
    loop {
        let msg = match tokio::time::timeout(idle, ws.next()).await {
            Err(_) => {
                return format!("{} 秒内没有收到任何数据", idle.as_secs());
            }
            Ok(None) => return "上游关闭了连接".to_string(),
            Ok(Some(Err(e))) => return format!("读取失败：{e}"),
            Ok(Some(Ok(m))) => m,
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(frame) => {
                return match frame {
                    Some(f) => format!("上游关闭：{} {}", u16::from(f.code), f.reason),
                    None => "上游关闭".to_string(),
                };
            }
            // Ping 由 tungstenite 在下一次读时自动回 Pong，这里什么都不用做；
            // 但它仍然算作"有动静"，已经重置了上面的空闲计时。
            _ => continue,
        };

        let applied = match kind {
            StreamKind::Depth => parse_depth_event(text.as_str()).map(|book| {
                tx.send_modify(|v| {
                    v.book = Some(book);
                    v.depth_link = LinkState::Live;
                });
            }),
            StreamKind::Trades => parse_agg_trade(text.as_str()).map(|trade| {
                tx.send_if_modified(|v| {
                    let was_live = v.trades_link == LinkState::Live;
                    v.trades_link = LinkState::Live;
                    v.push_trade(trade) || !was_live
                });
            }),
            StreamKind::Kline(interval) => {
                parse_kline_event(text.as_str(), symbol, interval).map(|candle| {
                    tx.send_if_modified(|v| {
                        let was_live = v.kline_link == LinkState::Live;
                        v.kline_link = LinkState::Live;
                        v.push_candle(candle) || !was_live
                    });
                })
            }
        };
        if let Err(e) = applied {
            if parse_warned {
                tracing::debug!(symbol, stream = kind.label(), error = %e, "推送消息无法解析");
            } else {
                parse_warned = true;
                tracing::warn!(symbol, stream = kind.label(), error = %e, "推送消息无法解析，已丢弃");
            }
        }
    }
}

/// 一条上游连接的完整生命周期：连接 → 读 → 断开 → 退避 → 重连，直到没人订阅。
async fn run_link(
    hub: Arc<Hub>,
    symbol: String,
    key: String,
    kind: StreamKind,
    tx: watch::Sender<MarketView>,
) {
    let config = &hub.config;
    let url = stream_url(&config.base, kind, &symbol);
    let mut backoff = Backoff::new(config.backoff_initial, config.backoff_max);
    let retire = until_unused(&hub, &key, &tx);
    tokio::pin!(retire);

    tracing::info!(symbol, stream = kind.label(), url, "行情推送：开始连接");

    loop {
        // 冷却期内不握手——封禁按 IP 记，握手也算请求。
        if let Some(ms) = hub.cooldown.remaining_ms() {
            set_link(&tx, kind, LinkState::CoolingDown { remaining_ms: ms });
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(ms)) => continue,
                _ = &mut retire => break,
            }
        }

        set_link(&tx, kind, LinkState::Connecting);
        let connected = tokio::select! {
            r = connect_once(config, &url) => r,
            _ = &mut retire => break,
        };

        let reason = match connected {
            Ok(mut ws) => {
                if matches!(kind, StreamKind::Kline(_)) {
                    tx.send_modify(|v| {
                        v.kline_generation = v.kline_generation.saturating_add(1);
                        v.candles.clear();
                    });
                }
                // 成交连接一连上就算在线：冷门交易对可能很久没有成交，"没数据"
                // 不代表断了（真断了由空闲超时发现）。盘口按固定频率推送，
                // 所以等第一帧到了才算在线——路由路径放错时它会一直停在"连接中"。
                if kind == StreamKind::Trades {
                    set_link(&tx, kind, LinkState::Live);
                }
                let started = Instant::now();
                let idle = config.idle_timeout(kind);
                let reason = tokio::select! {
                    r = pump(&mut ws, kind, &symbol, &tx, idle) => r,
                    _ = &mut retire => {
                        let _ = ws.close(None).await;
                        break;
                    }
                };
                if started.elapsed() >= config.stable_after {
                    backoff.reset();
                }
                reason
            }
            Err(ConnectError::RateLimited {
                retry_after_ms,
                status,
            }) => {
                tracing::warn!(
                    symbol,
                    stream = kind.label(),
                    status,
                    retry_after_ms,
                    "行情推送握手被限流，暂停所有请求"
                );
                hub.cooldown.arm_ms(retry_after_ms);
                continue;
            }
            Err(ConnectError::Other(reason)) => reason,
        };

        let delay = backoff.next_delay();
        let retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        tracing::warn!(
            symbol,
            stream = kind.label(),
            attempt = backoff.attempt,
            retry_in_ms,
            reason = %reason,
            "行情推送断开，将重连"
        );
        set_link(
            &tx,
            kind,
            LinkState::Retrying {
                attempt: backoff.attempt,
                retry_in_ms,
                reason,
            },
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = &mut retire => break,
        }
    }

    tracing::info!(
        symbol,
        stream = kind.label(),
        "行情推送：无人订阅，已关闭上游连接"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::SinkExt;
    use rust_decimal_macros::dec;

    const DEPTH_FRAME: &str = r#"{"stream":"ethusdc@depth20","data":{"e":"depthUpdate","E":1790354340123,"T":1790354340120,"s":"ETHUSDC","U":1,"u":2,"pu":0,"b":[["2687.41","3.5"],["2687.40","1.2"]],"a":[["2687.42","0.8"],["2687.45","4.0"]]}}"#;
    const TRADE_FRAME: &str = r#"{"stream":"ethusdc@aggTrade","data":{"e":"aggTrade","E":1790354340200,"s":"ETHUSDC","a":42,"p":"2687.42","q":"0.150","nq":"0.150","f":100,"l":101,"T":1790354340199,"m":true}}"#;

    const KLINE_FRAME: &str = r#"{"stream":"ethusdc@kline_15m","data":{"e":"kline","E":1790354340200,"s":"ETHUSDC","k":{"t":1790353800000,"s":"ETHUSDC","i":"15m","o":"2680.00","h":"2690.12","l":"2678.01","c":"2687.42","v":"123.150","x":false}}}"#;

    #[test]
    fn kline_parser_keeps_decimal_precision_and_validates_subscription() {
        let bar = parse_kline_event(KLINE_FRAME, "ETHUSDC", Interval::M15).unwrap();
        assert_eq!(bar.candle.open_time.timestamp_millis(), 1_790_353_800_000);
        assert_eq!(bar.event_ms, 1_790_354_340_200);
        assert_eq!(bar.candle.close, dec!(2687.42));
        assert_eq!(bar.candle.volume, dec!(123.150));
        assert!(!bar.candle.closed);
        assert!(parse_kline_event(KLINE_FRAME, "BTCUSDC", Interval::M15).is_err());
        assert!(parse_kline_event(KLINE_FRAME, "ETHUSDC", Interval::M1).is_err());
        assert!(
            parse_kline_event(
                &KLINE_FRAME.replace("2687.42", "NaN"),
                "ETHUSDC",
                Interval::M15
            )
            .is_err()
        );
        assert!(parse_kline_event(TRADE_FRAME, "ETHUSDC", Interval::M15).is_err());
        assert_eq!(
            stream_url(
                PRODUCTION_STREAM_URL,
                StreamKind::Kline(Interval::M15),
                "ETHUSDC"
            ),
            "wss://fstream.binance.com/market/stream?streams=ethusdc@kline_15m"
        );
    }

    #[test]
    fn kline_rollover_keeps_final_snapshot_and_drops_stale_updates() {
        let mut view = MarketView::default();
        let first = parse_kline_event(KLINE_FRAME, "ETHUSDC", Interval::M15).unwrap();
        assert!(view.push_candle(first.clone()));
        assert!(!view.push_candle(first.clone()));
        let mut stale = first.clone();
        stale.event_ms -= 1;
        stale.candle.close = dec!(1);
        assert!(!view.push_candle(stale));
        let mut closed = first.clone();
        closed.candle.closed = true;
        assert!(view.push_candle(closed));
        let mut next = first.clone();
        next.candle.open_time += chrono::Duration::minutes(15);
        next.event_ms += 900_000;
        assert!(view.push_candle(next));
        assert!(!view.push_candle(first));
        assert_eq!(view.candles.len(), 2);
        assert!(view.candles[0].candle.closed, "节流不能吞掉上一根收盘帧");
    }

    #[tokio::test]
    async fn kline_subscribers_share_per_interval_upstream_and_retire() {
        let (base, conns) = fake_stream_server().await;
        let streams = MarketStreams::with_config(StreamConfig::local_test(base), Cooldown::new());
        let mut a = streams.subscribe_klines("ETHUSDC", Interval::M15);
        wait_for(&mut a, |v| v.kline_link == LinkState::Live).await;
        let b = streams.subscribe_klines("ethusdc", Interval::M15);
        assert_eq!(b.borrow().kline_generation, 1);
        assert_eq!(b.borrow().candles.len(), 1);
        assert_eq!(conns.load(std::sync::atomic::Ordering::SeqCst), 1);
        let c = streams.subscribe_klines("ETHUSDC", Interval::M1);
        assert_eq!(streams.active_feeds(), 2, "不同周期独立共享");
        drop(a);
        drop(b);
        drop(c);
        tokio::time::timeout(Duration::from_secs(3), async {
            while streams.active_feeds() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("无人订阅应关闭 K 线连接");
    }

    #[tokio::test]
    async fn kline_reconnect_advances_generation_for_history_repair() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for index in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                ws.send(Message::Text(KLINE_FRAME.into())).await.unwrap();
                if index == 0 {
                    ws.close(None).await.unwrap();
                } else {
                    while let Some(Ok(_)) = ws.next().await {}
                }
            }
        });
        let streams = MarketStreams::with_config(
            StreamConfig::local_test(format!("ws://{addr}")),
            Cooldown::new(),
        );
        let mut rx = streams.subscribe_klines("ETHUSDC", Interval::M15);
        wait_for(&mut rx, |v| {
            v.kline_generation == 2 && v.kline_link == LinkState::Live
        })
        .await;
        assert_eq!(rx.borrow().candles.len(), 1, "新会话只保留重新收到的快照");
    }

    // ---- 地址与白名单 ----

    /// 盘口走 `/public`、成交走 `/market`。放错路径连得上但收不到数据。
    #[test]
    fn urls_use_routed_paths_and_lowercase_symbols() {
        assert_eq!(
            stream_url(PRODUCTION_STREAM_URL, StreamKind::Depth, "ETHUSDC"),
            "wss://fstream.binance.com/public/stream?streams=ethusdc@depth20"
        );
        assert_eq!(
            stream_url(PRODUCTION_STREAM_URL, StreamKind::Trades, "ETHUSDC"),
            "wss://fstream.binance.com/market/stream?streams=ethusdc@aggTrade"
        );
    }

    #[test]
    fn production_stream_endpoint_is_allowed() {
        for kind in [StreamKind::Depth, StreamKind::Trades] {
            let url = stream_url(PRODUCTION_STREAM_URL, kind, "ETHUSDC");
            assert!(stream_endpoint_allowed(&url).is_ok(), "{url}");
        }
    }

    #[test]
    fn stream_whitelist_rejects_other_hosts_and_plaintext() {
        for bad in [
            "wss://evil.example/public/stream",
            "wss://fstream.binance.com.evil.example/public/stream",
            "ws://fstream.binance.com/public/stream",
            "https://fstream.binance.com/public/stream",
            "not a url",
        ] {
            assert!(stream_endpoint_allowed(bad).is_err(), "{bad} 不应被放行");
        }
    }

    /// 推送主机**不能**顺带进入签名白名单——那是凭据可达范围的边界。
    #[test]
    fn stream_host_is_not_a_signing_endpoint() {
        assert!(
            crate::signing::endpoint_allowed("https://fstream.binance.com/fapi/v1/order").is_err()
        );
    }

    // ---- 解析 ----

    #[test]
    fn depth_frame_parses_from_combined_stream() {
        let book = parse_depth_event(DEPTH_FRAME).expect("应能解析");
        assert_eq!(book.bid, dec!(2687.41));
        assert_eq!(book.ask, dec!(2687.42));
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.asks[1], (dec!(2687.45), dec!(4.0)));
        assert_eq!(book.at.timestamp_millis(), 1_790_354_340_123);
    }

    /// 直连单流时没有外壳，字段名也可能是现货风格的 `bids`/`asks`。
    #[test]
    fn depth_frame_parses_without_envelope_and_with_long_names() {
        let book =
            parse_depth_event(r#"{"bids":[["1.5","2"]],"asks":[["1.6","3"]]}"#).expect("应能解析");
        assert_eq!(book.bid, dec!(1.5));
        assert_eq!(book.ask, dec!(1.6));
    }

    /// 坏数值整帧丢弃，不能悄悄少一档。
    #[test]
    fn depth_frame_with_bad_number_is_rejected() {
        let bad = r#"{"b":[["abc","1"]],"a":[]}"#;
        assert!(parse_depth_event(bad).is_err());
    }

    #[test]
    fn agg_trade_parses_from_combined_stream() {
        let t = parse_agg_trade(TRADE_FRAME).expect("应能解析");
        assert_eq!(t.trade_id, 42);
        assert_eq!(t.price.get(), dec!(2687.42));
        assert_eq!(t.quantity.get(), dec!(0.150));
        assert!(t.is_buyer_maker);
        assert_eq!(t.at.timestamp_millis(), 1_790_354_340_199);
    }

    /// 连错了流（例如把盘口帧当成交解析）必须报错，而不是解析出一笔空成交。
    #[test]
    fn depth_frame_is_not_a_trade() {
        assert!(parse_agg_trade(DEPTH_FRAME).is_err());
    }

    // ---- 视图 ----

    fn trade(id: u64) -> AggTrade {
        AggTrade {
            trade_id: id,
            price: Price::new(dec!(1)),
            quantity: Qty::new(dec!(1)),
            is_buyer_maker: false,
            at: Utc::now(),
        }
    }

    /// 重连后上游可能重发已见过的成交；它们不能在成交流里出现两次。
    #[test]
    fn duplicate_and_older_trades_are_dropped() {
        let mut v = MarketView::default();
        assert!(v.push_trade(trade(10)));
        assert!(v.push_trade(trade(11)));
        assert!(!v.push_trade(trade(11)), "重复成交");
        assert!(!v.push_trade(trade(5)), "更旧的成交");
        let ids: Vec<u64> = v.trades.iter().map(|t| t.trade_id).collect();
        assert_eq!(ids, vec![11, 10], "新的在前");
    }

    /// REST 补底的成交比推送来的旧，必须插到后面，而不是被当成"更旧"丢掉。
    #[test]
    fn seeded_trades_merge_behind_live_ones() {
        let mut v = MarketView::default();
        v.push_trade(trade(100));
        v.merge_trades([trade(98), trade(99), trade(100)]);
        let ids: Vec<u64> = v.trades.iter().map(|t| t.trade_id).collect();
        assert_eq!(ids, vec![100, 99, 98]);
        assert!(v.push_trade(trade(101)), "补底之后推送照常追加");
    }

    #[test]
    fn recent_trades_are_capped() {
        let mut v = MarketView::default();
        for id in 0..(RECENT_TRADES_CAP as u64 + 20) {
            v.push_trade(trade(id));
        }
        assert_eq!(v.trades.len(), RECENT_TRADES_CAP);
    }

    // ---- 退避 ----

    #[test]
    fn backoff_grows_to_cap_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        let delays: Vec<Duration> = (0..6).map(|_| b.next_delay()).collect();
        // 抖动最多 +20%，所以用区间判断
        let within = |d: Duration, base: u64| {
            d >= Duration::from_secs(base) && d <= Duration::from_millis(base * 1_200)
        };
        assert!(within(delays[0], 1), "{delays:?}");
        assert!(within(delays[1], 2), "{delays:?}");
        assert!(within(delays[2], 4), "{delays:?}");
        for d in &delays[3..] {
            assert!(*d <= Duration::from_secs(8), "不能超过上限：{delays:?}");
        }
        assert_eq!(b.attempt, 6);

        b.reset();
        assert!(within(b.next_delay(), 1), "复位后从初始值重来");
    }

    // ---- 端到端：本地假上游 ----

    /// 本地假推送服务。每条连接推一帧盘口、一帧成交，然后保持连接。
    async fn fake_stream_server() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地端口");
        let addr = listener.local_addr().expect("本地地址");
        let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = conns.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else {
                        return;
                    };
                    // 两种帧都发：每条连接只认自己那一种，另一种走"无法解析、丢弃"。
                    for frame in [DEPTH_FRAME, TRADE_FRAME, KLINE_FRAME] {
                        let _ = ws.send(Message::Text(frame.into())).await;
                    }
                    while let Some(Ok(_)) = ws.next().await {}
                });
            }
        });
        (format!("ws://{addr}"), conns)
    }

    async fn wait_for(rx: &mut watch::Receiver<MarketView>, pred: impl Fn(&MarketView) -> bool) {
        let fut = async {
            loop {
                if pred(&rx.borrow_and_update()) {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(3), fut)
            .await
            .expect("等待视图更新超时");
    }

    /// 订阅 → 两条连接都收到数据 → 视图为 Live；再次订阅复用同一份上游；
    /// 所有订阅者离开后，上游在宽限期后关闭。
    #[tokio::test]
    async fn subscribers_share_one_upstream_and_it_retires_when_unused() {
        let (base, conns) = fake_stream_server().await;
        let streams = MarketStreams::with_config(StreamConfig::local_test(base), Cooldown::new());

        let mut a = streams.subscribe("ETHUSDC");
        wait_for(&mut a, |v| v.is_live()).await;
        {
            let v = a.borrow();
            assert_eq!(v.book.as_ref().map(|b| b.bid), Some(dec!(2687.41)));
            assert_eq!(v.trades.front().map(|t| t.trade_id), Some(42));
        }

        // 第二个订阅者（第二个标签页）不新建上游连接。
        let b = streams.subscribe("ethusdc");
        assert!(b.borrow().is_live(), "新订阅者应立即拿到最新视图");
        assert_eq!(
            conns.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "盘口 + 成交各一条"
        );
        assert_eq!(streams.active_feeds(), 1);

        drop(a);
        drop(b);
        tokio::time::timeout(Duration::from_secs(3), async {
            while streams.active_feeds() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("无人订阅后上游应关闭");
    }

    /// 握手被 429 拒绝：冷却必须被点亮（REST 也会一起停），且视图显示冷却中。
    #[tokio::test]
    async fn rate_limited_handshake_arms_shared_cooldown() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地端口");
        let addr = listener.local_addr().expect("本地地址");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
            }
        });

        let cooldown = Cooldown::new();
        let streams = MarketStreams::with_config(
            StreamConfig::local_test(format!("ws://{addr}")),
            cooldown.clone(),
        );
        let mut rx = streams.subscribe("ETHUSDC");
        wait_for(&mut rx, |v| {
            matches!(v.depth_link, LinkState::CoolingDown { .. })
                || matches!(v.trades_link, LinkState::CoolingDown { .. })
        })
        .await;
        let remaining = cooldown.remaining_ms().expect("应已进入冷却");
        assert!(
            remaining > 5_000 && remaining <= 7_000,
            "冷却应来自 Retry-After: 7，实际 {remaining}ms"
        );
    }
}
