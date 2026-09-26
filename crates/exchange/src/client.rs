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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use domain::ServiceMode;
use rust_decimal::Decimal;

use crate::binance::{
    AcceptedOrder, AccountFeeResponse, ContractSpec, ExchangeInfoResponse, OrderRequest,
    OrderResponse, RateLimitHint, classify_api_error_with, find_contract, parse_all_contracts,
};
use crate::cooldown::{Cooldown, DEFAULT_RETRY_AFTER_MS, parse_banned_until_ms};
use crate::error::ExchangeError;
use crate::signing::{Credentials, endpoint_allowed};

/// 公开行情的缓存条目。
#[derive(Clone)]
struct CacheEntry {
    /// 原始响应正文。缓存**未解析**的正文而不是领域对象：解析逻辑在
    /// `market.rs`，缓存放在解析之前，才不会因为新增字段而漏缓存。
    body: String,
    /// 过期时刻（毫秒时间戳）。
    expires_at_ms: i64,
}

/// 免重复请求的公开行情缓存。
///
/// # 为什么这是"限流修复"而不是"性能优化"
///
/// 币安的权重限制是**按 IP** 计的，与本进程开了几个标签页、几个前端无关。
/// 没有共享缓存时，请求量 = 前端数量 × 每个前端的轮询频率；有缓存后，
/// 上游请求量只由 TTL 决定，与请求方数量解耦。这是把"多开一个页面就多一份
/// 超限风险"这个结构性缺陷去掉。
///
/// # 缓存的边界
///
/// 只缓存**公开**行情（K 线、盘口、成交流）。签名请求（下单、撤单、查单）
/// 绝不能被缓存——那会让撤单返回上一次的"成功"，把状态机带进错误分支。
#[derive(Default)]
struct MarketCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    /// 每个键一把异步锁，用来做单飞。
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl MarketCache {
    /// 读缓存。过期视为未命中。
    fn get(&self, key: &str) -> Option<String> {
        let entries = self.entries.lock().ok()?;
        let e = entries.get(key)?;
        if e.expires_at_ms <= Utc::now().timestamp_millis() {
            return None;
        }
        Some(e.body.clone())
    }

    /// 写入缓存，并顺手清掉过期条目。
    ///
    /// 清理是**每次写入时顺带**做的：单独起一个清理任务要引入定时器与
    /// 任务生命周期，而这里的键空间很小（交易对 × 周期 × 参数的组合），
    /// 写入时线性扫一遍足够。
    fn put(&self, key: String, body: String, ttl_ms: i64) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let now = Utc::now().timestamp_millis();
        entries.retain(|_, e| e.expires_at_ms > now);
        entries.insert(
            key,
            CacheEntry {
                body,
                expires_at_ms: now.saturating_add(ttl_ms),
            },
        );
    }

    /// 取某个键的异步锁，用于单飞。
    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let Ok(mut locks) = self.locks.lock() else {
            // 锁表中毒时退化成一把新锁：最坏情况是并发打一次上游，
            // 而不是整个行情接口不可用。
            return Arc::new(tokio::sync::Mutex::new(()));
        };
        locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

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
///
/// # 共享状态
///
/// `cooldown` 与 `cache` 都是 `Arc` 支撑的，**克隆共享同一份**。这一点很关键：
/// 封禁记在 IP 上，不区分是哪个 `BinanceClient` 实例发的请求。如果每个实例
/// 各自维护冷却与缓存，多副本部署（或同一进程里多个调用点各建一个客户端）
/// 就会重新把请求量放大回封禁前的水平。
pub struct BinanceClient {
    http: reqwest::Client,
    base: String,
    creds: Option<Credentials>,
    recv_window_ms: u64,
    /// 上游限流冷却。
    cooldown: Cooldown,
    /// 公开行情缓存。
    cache: Arc<MarketCache>,
}

impl BinanceClient {
    /// 构造只读客户端（不需要凭据）。用于拉取公开的合约规则。
    pub fn public(base: &str) -> Result<Self, ExchangeError> {
        Self::public_with_cooldown(base, Cooldown::new())
    }

    /// 构造只读客户端，并注入一份**共享**冷却状态。
    ///
    /// 多个客户端指向同一个 IP 的同一个上游时必须共用冷却——否则 A 客户端
    /// 撞到 418 之后，B 客户端仍会继续打，封禁被不断续期。
    pub fn public_with_cooldown(base: &str, cooldown: Cooldown) -> Result<Self, ExchangeError> {
        endpoint_allowed(base)?;
        Self::build(base, None, cooldown)
    }

    /// 组装客户端。**不做白名单校验**——校验是调用方的责任，见下面三个
    /// 构造器。抽出来只是为了让"跳过校验"这件事在一个地方被看见。
    fn build(
        base: &str,
        creds: Option<Credentials>,
        cooldown: Cooldown,
    ) -> Result<Self, ExchangeError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::Fatal(format!("构造 HTTP 客户端失败：{e}")))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            creds,
            recv_window_ms: DEFAULT_RECV_WINDOW_MS,
            cooldown,
            cache: Arc::new(MarketCache::default()),
        })
    }

    /// **仅测试用**：构造指向本地假上游的只读客户端，跳过 endpoint 白名单。
    ///
    /// # 为什么需要这个例外
    ///
    /// 限流与缓存的回归测试必须有一个**能数出请求次数**的上游，只有本地
    /// 回环地址能做到（打真实币安既慢又会触发真封禁）。但白名单是安全边界
    /// ——签名请求会把 API Key 发到目标主机——不能为了测试放宽
    /// [`endpoint_allowed`]，所以例外只在这里开一个口子。
    ///
    /// # 影响范围
    ///
    /// 只在 `cfg(test)` 下编译，生产二进制里不存在这个方法；构造出的客户端
    /// **不带凭据**，签名路径会先被 `require_creds` 拒绝，因此即便有人误用
    /// 也不可能把密钥发到白名单外的主机。
    #[cfg(test)]
    pub(crate) fn for_local_test(base: &str) -> Result<Self, ExchangeError> {
        Self::build(base, None, Cooldown::new())
    }

    /// 构造签名客户端。
    ///
    /// `mode` 参数存在的意义是让调用点显式声明意图。将来加入 `Paper` 时，
    /// 这里会是"模拟盘无法构造签名客户端"的落点。
    pub fn signed(base: &str, creds: Credentials, mode: Mode) -> Result<Self, ExchangeError> {
        Self::signed_with_cooldown(base, creds, mode, Cooldown::new())
    }

    /// 构造签名客户端，并注入共享冷却状态。
    pub fn signed_with_cooldown(
        base: &str,
        creds: Credentials,
        mode: Mode,
        cooldown: Cooldown,
    ) -> Result<Self, ExchangeError> {
        let Mode::Live = mode;
        endpoint_allowed(base)?;
        Self::build(base, Some(creds), cooldown)
    }

    /// 共享冷却状态的句柄。
    pub fn cooldown(&self) -> &Cooldown {
        &self.cooldown
    }

    /// 把请求方绑到本客户端的共享冷却上。
    ///
    /// 用于把同一进程里其它客户端（例如后台任务自己建的）统一到一份冷却。
    pub fn share_cooldown_with(&self, other: &BinanceClient) {
        other.cooldown.adopt(&self.cooldown);
    }

    /// 当前是否处于限流冷却中，以及还需等待多久。
    pub fn cooldown_remaining_ms(&self) -> Option<u64> {
        self.cooldown.remaining_ms()
    }

    /// 冷却检查。返回 `Err` 表示**不应发请求**。
    fn check_cooldown(&self, path: &str) -> Result<(), ExchangeError> {
        match self.cooldown.remaining_ms() {
            Some(ms) => {
                tracing::debug!(path, retry_after_ms = ms, "限流冷却中，跳过上游请求");
                Err(ExchangeError::RateLimited { retry_after_ms: ms })
            }
            None => Ok(()),
        }
    }

    /// 处理一次非 2xx 响应：记录日志、按需进入冷却、返回分类后的错误。
    ///
    /// 四种请求（公开 GET、签名 GET/POST/DELETE）共用这一处，保证每次上游
    /// 失败都带着**路径与状态码**落日志——事故里 60 秒 91 次失败却看不出是
    /// 哪个接口、上游回了什么，就是因为只有部分路径记了日志。
    ///
    /// `retry_after` 须在 `resp.text()` 之前从响应头取出：`text()` 会消费
    /// 响应，之后拿不到头。
    fn upstream_failure(
        &self,
        method: &str,
        path: &str,
        status: u16,
        retry_after: Option<&str>,
        used_weight: Option<&str>,
        body: &str,
    ) -> ExchangeError {
        let (code, msg) = binance_error_fields(body);
        if status == 429 || status == 418 {
            // 顺序与 `classify_api_error_with` 保持一致：响应头优先于正文时间戳。
            let ms = crate::cooldown::parse_retry_after_opt(retry_after)
                .or_else(|| parse_banned_until_ms(body, Utc::now().timestamp_millis()))
                .unwrap_or(DEFAULT_RETRY_AFTER_MS);
            tracing::warn!(
                method,
                path,
                status,
                code,
                msg = %msg,
                used_weight_1m = used_weight.unwrap_or("-"),
                retry_after_ms = ms,
                "上游限流，暂停所有请求"
            );
            self.cooldown.arm_ms(ms);
        } else {
            tracing::warn!(
                method,
                path,
                status,
                code,
                msg = %msg,
                used_weight_1m = used_weight.unwrap_or("-"),
                "上游请求失败"
            );
        }
        classify_api_error_with(status, Self::rate_hint(retry_after, body))
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

    /// 把可选响应头转成 `classify_api_error_with` 需要的借用。
    fn rate_hint<'a>(retry_after: Option<&'a str>, body: &'a str) -> RateLimitHint<'a> {
        RateLimitHint { retry_after, body }
    }

    /// 发送签名请求（POST，参数放在查询串里）。
    async fn post_signed(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<String, ExchangeError> {
        let creds = self.require_creds()?;
        // 冷却期内直接拒绝，不发请求。放在签名之前会浪费一次 HMAC，
        // 放在这里则是"已签名但未发送"——签名没有副作用，可以接受。
        self.check_cooldown(path)?;
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
                log_transport_failure("POST", path, &e);
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
        let retry_after = header_string(&resp, "retry-after");
        let used_weight = header_string(&resp, "x-mbx-used-weight-1m");
        let body = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            self.cooldown.clear_if_elapsed();
            Ok(body)
        } else {
            Err(self.upstream_failure(
                "POST",
                path,
                status,
                retry_after.as_deref(),
                used_weight.as_deref(),
                &body,
            ))
        }
    }

    /// 发送签名 GET 请求。
    async fn get_signed(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<String, ExchangeError> {
        let creds = self.require_creds()?;
        // 与 post/delete 一致：冷却期内直接拒绝，不发请求。漏掉这一处会让
        // 查询类请求在封禁期间继续打上游，把 2 分钟的封禁续成更久。
        self.check_cooldown(path)?;
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
                log_transport_failure("GET", path, &e);
                if e.is_timeout() {
                    ExchangeError::Unknown(format!("请求超时：{e}"))
                } else {
                    ExchangeError::Definitive(format!("请求失败：{e}"))
                }
            })?;

        let status = resp.status().as_u16();
        let retry_after = header_string(&resp, "retry-after");
        let used_weight = header_string(&resp, "x-mbx-used-weight-1m");
        let body = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            self.cooldown.clear_if_elapsed();
            Ok(body)
        } else {
            Err(self.upstream_failure(
                "GET",
                path,
                status,
                retry_after.as_deref(),
                used_weight.as_deref(),
                &body,
            ))
        }
    }

    /// 发送签名 DELETE 请求（撤单用）。
    async fn delete_signed(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<String, ExchangeError> {
        let creds = self.require_creds()?;
        self.check_cooldown(path)?;
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
                log_transport_failure("DELETE", path, &e);
                if e.is_timeout() {
                    // 撤单超时同样不能重试——撤单可能已经生效，
                    // 重试会得到 -2011 或被误导。
                    ExchangeError::Unknown(format!("撤单请求超时：{e}"))
                } else {
                    ExchangeError::Definitive(format!("撤单请求失败：{e}"))
                }
            })?;

        let status = resp.status().as_u16();
        let retry_after = header_string(&resp, "retry-after");
        let used_weight = header_string(&resp, "x-mbx-used-weight-1m");
        let body = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            self.cooldown.clear_if_elapsed();
            Ok(body)
        } else {
            Err(self.upstream_failure(
                "DELETE",
                path,
                status,
                retry_after.as_deref(),
                used_weight.as_deref(),
                &body,
            ))
        }
    }

    /// 发送公开 GET 请求（无需签名）。
    pub(crate) async fn get_public(
        &self,
        path: &str,
        query: &str,
    ) -> Result<String, ExchangeError> {
        self.get_public_cached(path, query, 0).await
    }

    /// 发送公开 GET 请求，并带 TTL 缓存与单飞。
    ///
    /// `ttl_ms == 0` 表示不缓存（例如合约规则这种一次性的调用）。
    ///
    /// # 命中顺序
    ///
    /// 1. 缓存命中 → 直接返回，**不检查冷却、不打上游**；
    /// 2. 未命中 → 取该键的单飞锁 → 再查一次缓存（可能已被前一个请求填上）
    ///    → 仍无 → 检查冷却 → 打上游 → 写缓存。
    ///
    /// 先查缓存再查冷却是刻意的：封禁期间缓存里的数据仍然可以服务界面，
    /// 让用户看到"20 分钟前的盘口"远好过看到一个错误页。
    pub(crate) async fn get_public_cached(
        &self,
        path: &str,
        query: &str,
        ttl_ms: i64,
    ) -> Result<String, ExchangeError> {
        if ttl_ms <= 0 {
            return self.fetch_public(path, query).await;
        }
        let key = format!("{path}?{query}");
        if let Some(body) = self.cache.get(&key) {
            return Ok(body);
        }

        // 单飞：并发请求同一个键时只有一个真的打上游，其余等它填完缓存。
        let lock = self.cache.lock_for(&key);
        let _guard = lock.lock().await;

        // 拿到锁后重查一次——等锁期间前一个请求可能已经填好了。
        if let Some(body) = self.cache.get(&key) {
            return Ok(body);
        }

        let body = self.fetch_public(path, query).await?;
        self.cache.put(key, body.clone(), ttl_ms);
        Ok(body)
    }

    /// 真正发出公开 GET 请求。
    async fn fetch_public(&self, path: &str, query: &str) -> Result<String, ExchangeError> {
        self.check_cooldown(path)?;
        let url = if query.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        let resp = self.http.get(&url).send().await.map_err(|e| {
            log_transport_failure("GET", path, &e);
            if e.is_timeout() {
                ExchangeError::Unknown(format!("请求超时：{e}"))
            } else {
                ExchangeError::Definitive(format!("请求失败：{e}"))
            }
        })?;
        let status = resp.status().as_u16();
        let used_weight = header_string(&resp, "x-mbx-used-weight-1m");
        let retry_after = header_string(&resp, "retry-after");
        let body = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            self.cooldown.clear_if_elapsed();
            // 已用权重是判断"离超限还有多远"的唯一可见信号，
            // 记录下来才能在下一次封禁前发现趋势。
            if let Some(w) = used_weight {
                tracing::debug!(path, used_weight_1m = %w, "公开行情请求成功");
            }
            Ok(body)
        } else {
            Err(self.upstream_failure(
                "GET",
                path,
                status,
                retry_after.as_deref(),
                used_weight.as_deref(),
                &body,
            ))
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

    /// 读取某交易对在交易所侧的保证金模式与杠杆。
    ///
    /// # 为什么走 REST 而不是 WS
    ///
    /// ws-fapi 的 `account.position` 有等价能力，但仓库里还没有 ws-fapi 的
    /// 请求-响应客户端，而这里只在**实盘对账时调用一次**（不轮询）。长期持续
    /// 监控应该改用用户数据流的 `ACCOUNT_UPDATE` 推送。
    ///
    /// 注意：v3 版 `positionRisk` 已去掉 `marginType` 字段，所以这里用 v2。
    pub async fn position_margin_state(
        &self,
        symbol: &str,
    ) -> Result<crate::binance::SymbolMarginState, ExchangeError> {
        let body = self
            .get_signed(
                "/fapi/v2/positionRisk",
                vec![("symbol", symbol.to_string())],
            )
            .await?;
        Ok(crate::binance::parse_position_margin(&body, symbol))
    }

    /// 请求把某交易对的保证金模式切换为**全仓**。
    ///
    /// # 调用前提
    ///
    /// 币安要求该交易对**没有持仓也没有挂单**才能切换，否则分别返回
    /// -4048 / -4047。调用方应只在空仓且无挂单时调用；**不要为了切换而
    /// 先撤单或平仓**——那是另一个决策，必须由操作者做。
    ///
    /// 走 REST 的理由同上（ws-fapi 没有对应方法）。权重 1。
    pub async fn set_margin_type_crossed(
        &self,
        symbol: &str,
    ) -> Result<crate::binance::MarginTypeChange, ExchangeError> {
        let result = self
            .post_signed(
                "/fapi/v1/marginType",
                vec![
                    ("symbol", symbol.to_string()),
                    ("marginType", "CROSSED".to_string()),
                ],
            )
            .await;
        crate::binance::margin_type_change_outcome(result)
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

/// 读取一个响应头，返回 `Option<String>`。
///
/// **必须在 `resp.text()` 之前调用**——`text()` 会消费响应体，之后
/// `resp` 已被借用走。响应头不是 UTF-8 或缺失时返回 `None`，不报错：
/// 缺头是常态（测试网、代理），不该让整个请求失败。
fn header_string(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)?
        .to_str()
        .ok()
        .map(|s| s.trim().to_string())
}

/// 日志里保留的上游正文最大字符数。币安的错误正文是一行 JSON，200 字足够；
/// 截断是为了防止网关返回整页 HTML 时刷屏。
const LOG_BODY_MAX_CHARS: usize = 200;

/// 从错误正文里取出币安错误码与说明，供日志使用。
///
/// 不是 `{code, msg}` 时返回截断后的原文——网关层（CDN、WAF）的错误页
/// 不是这个格式，但它同样是排查依据。
fn binance_error_fields(body: &str) -> (Option<i64>, String) {
    match serde_json::from_str::<crate::binance::BinanceError>(body) {
        Ok(e) => (Some(e.code), e.msg),
        Err(_) => (None, body.chars().take(LOG_BODY_MAX_CHARS).collect()),
    }
}

/// 记录没拿到 HTTP 响应的失败（超时、连接失败等）。
///
/// 只记底层原因（`source()`），不记 `reqwest::Error` 本身：后者的 `Display`
/// 带完整 URL，签名请求的查询串里有时间戳与签名，不应进日志。
fn log_transport_failure(method: &str, path: &str, e: &reqwest::Error) {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else {
        "other"
    };
    let cause = std::error::Error::source(e)
        .map(|s| s.to_string())
        .unwrap_or_default();
    tracing::warn!(method, path, kind, cause = %cause, "上游请求未得到响应");
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

    // ---- 限流贯通与缓存：用一个本地假上游验证 ----
    //
    // 这些测试打的是 127.0.0.1 上的临时端口，不碰币安。它们验证的是
    // 事故里的两条根因：真实 `Retry-After` 有没有被保留、封禁期间还会不会
    // 继续发请求。

    /// 一次测试用的假上游：按脚本依次返回响应，并记录收到的请求数。
    struct FakeUpstream {
        base: String,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeUpstream {
        /// 每次请求都返回同一个响应。
        async fn always(
            status: u16,
            retry_after: Option<&'static str>,
            body: &'static str,
        ) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("绑定本地端口");
            let addr = listener.local_addr().expect("取得本地地址");
            let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = hits.clone();

            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // 读掉请求头，避免客户端认为连接被提前关闭。
                    let mut buf = [0u8; 1024];
                    let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;

                    let reason = if status == 200 {
                        "OK"
                    } else {
                        "Too Many Requests"
                    };
                    let mut resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    if let Some(ra) = retry_after {
                        resp.push_str(&format!("Retry-After: {ra}\r\n"));
                    }
                    resp.push_str("\r\n");
                    resp.push_str(body);
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
                    let _ = tokio::io::AsyncWriteExt::flush(&mut sock).await;
                }
            });

            Self {
                base: format!("http://{addr}"),
                hits,
            }
        }

        /// 指向本地假上游的只读客户端。
        ///
        /// 刻意**不走** `public`：那条路会走 endpoint 白名单，而回环地址
        /// 不在白名单内——这是设计如此，见 `endpoint_allowed` 的测试。
        fn client(&self) -> BinanceClient {
            BinanceClient::for_local_test(&self.base).expect("构造本地测试客户端")
        }

        fn hits(&self) -> usize {
            self.hits.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// **事故回归**：418 的真实等待时间要从响应头进到客户端状态。
    #[tokio::test]
    async fn rate_limit_arms_shared_cooldown_with_real_duration() {
        let up = FakeUpstream::always(
            418,
            Some("1234"),
            r#"{"code":-1003,"msg":"Way too much request weight used; IP banned until 1790354159999."}"#,
        )
        .await;
        let c = up.client();

        let err = c.get_public("/fapi/v1/depth", "symbol=ETHUSDC").await;
        match err {
            Err(ExchangeError::RateLimited { retry_after_ms }) => {
                assert_eq!(retry_after_ms, 1_234_000, "必须是响应头给的 1234 秒");
            }
            other => panic!("应是限流错误：{other:?}"),
        }
        let remaining = c.cooldown_remaining_ms().expect("冷却必须已生效");
        assert!(
            remaining > 1_000_000,
            "冷却应约 20 分钟，实际 {remaining}ms"
        );
    }

    /// **最关键的一条**：冷却期内不再打上游。
    ///
    /// 事故的核心机制就是"429 之后继续打 → 升级成 418 封禁"。
    #[tokio::test]
    async fn no_upstream_request_is_sent_during_cooldown() {
        let up = FakeUpstream::always(429, Some("600"), "too many requests").await;
        let c = up.client();

        assert!(
            c.get_public("/fapi/v1/depth", "symbol=ETHUSDC")
                .await
                .is_err()
        );
        assert_eq!(up.hits(), 1, "第一次应真的打到上游");

        // 冷却生效后连续请求都不该离开本进程
        for _ in 0..5 {
            let e = c.get_public("/fapi/v1/depth", "symbol=ETHUSDC").await;
            assert!(
                matches!(e, Err(ExchangeError::RateLimited { .. })),
                "冷却期内应直接返回限流错误：{e:?}"
            );
        }
        assert_eq!(up.hits(), 1, "冷却期内不能再打上游——这是把封禁拖长的原因");
    }

    /// 缺 `Retry-After` 时用兜底值，但**仍然**要进入冷却。
    #[tokio::test]
    async fn missing_retry_after_still_arms_a_cooldown() {
        let up = FakeUpstream::always(429, None, "too many requests").await;
        let c = up.client();

        assert!(
            c.get_public("/fapi/v1/depth", "symbol=ETHUSDC")
                .await
                .is_err()
        );
        assert!(
            c.cooldown_remaining_ms().is_some(),
            "没有响应头也必须冷却，否则会在封禁期继续打"
        );
    }

    /// 冷却的克隆共享同一份状态——多调用点不会各自撞限流。
    #[test]
    fn cooldown_is_shared_across_clients() {
        let a = BinanceClient::public(PRODUCTION_URL).unwrap();
        let b = BinanceClient::public(PRODUCTION_URL).unwrap();
        a.cooldown().arm_ms(60_000);
        assert!(
            b.cooldown_remaining_ms().is_none(),
            "两个独立客户端默认各有冷却"
        );
        a.share_cooldown_with(&b);
        assert!(
            b.cooldown_remaining_ms().is_some(),
            "绑定后必须共享同一份冷却"
        );
    }

    /// 本地测试用的例外**没有**放宽生产路径：白名单本身照旧拒绝回环地址。
    ///
    /// 这条断言是上面 `for_local_test` 的护栏——如果哪天有人把例外挪进
    /// `endpoint_allowed`，这里会先失败。
    #[test]
    fn local_test_escape_hatch_does_not_weaken_the_whitelist() {
        assert!(
            BinanceClient::public("http://127.0.0.1:9999").is_err(),
            "生产构造器必须继续拒绝回环地址"
        );
        assert!(crate::signing::endpoint_allowed("http://127.0.0.1:9999").is_err());
        // 例外路径本身可用，但拿不到凭据
        let c = BinanceClient::for_local_test("http://127.0.0.1:9999").expect("测试构造器可用");
        assert!(
            !c.is_signed(),
            "本地测试客户端不带凭据，签名路径会被 require_creds 挡住"
        );
    }

    /// 缓存命中时上游只被打一次——这是"请求量与前端数量解耦"的根据。
    #[tokio::test]
    async fn cache_serves_repeated_reads_from_one_upstream_call() {
        let up = FakeUpstream::always(200, None, r#"[{"ok":true}]"#).await;
        let c = up.client();

        for _ in 0..10 {
            let body = c
                .get_public_cached("/fapi/v1/depth", "symbol=ETHUSDC&limit=20", 5_000)
                .await
                .expect("应该成功");
            assert_eq!(body, r#"[{"ok":true}]"#);
        }
        assert_eq!(
            up.hits(),
            1,
            "10 次读取只应产生 1 次上游请求，实际 {}",
            up.hits()
        );
    }

    /// 不同的查询参数必须是不同的缓存键。
    #[tokio::test]
    async fn cache_key_includes_query_parameters() {
        let up = FakeUpstream::always(200, None, "[]").await;
        let c = up.client();

        c.get_public_cached("/fapi/v1/depth", "symbol=ETHUSDC", 5_000)
            .await
            .unwrap();
        c.get_public_cached("/fapi/v1/klines", "symbol=ETHUSDC", 5_000)
            .await
            .unwrap();
        c.get_public_cached("/fapi/v1/depth", "symbol=BTCUSDC", 5_000)
            .await
            .unwrap();
        assert_eq!(up.hits(), 3, "路径或参数不同不能共用缓存");
    }

    /// TTL 为 0 表示不缓存——签名类/一次性调用不能用缓存。
    #[tokio::test]
    async fn zero_ttl_bypasses_cache() {
        let up = FakeUpstream::always(200, None, "[]").await;
        let c = up.client();
        for _ in 0..3 {
            c.get_public_cached("/fapi/v1/exchangeInfo", "", 0)
                .await
                .unwrap();
        }
        assert_eq!(up.hits(), 3, "TTL=0 必须每次都打上游");
    }

    /// 并发读取同一个键时只有一次上游请求（单飞），不能出现惊群。
    ///
    /// 这里**共用同一个客户端**（也就是同一份缓存与锁表）——事故里前端
    /// 多开标签页正好就是这个形状：同一个进程、同一个上游、同一批请求。
    /// 如果每个任务各建一个客户端，测的就不是这套机制了。
    #[tokio::test]
    async fn concurrent_reads_collapse_into_one_upstream_call() {
        let up = FakeUpstream::always(200, None, "[]").await;
        let c = std::sync::Arc::new(up.client());

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let c2 = c.clone();
            set.spawn(async move {
                c2.get_public_cached("/fapi/v1/depth", "symbol=ETHUSDC", 5_000)
                    .await
            });
        }
        while let Some(r) = set.join_next().await {
            assert!(r.expect("任务不应 panic").is_ok());
        }
        assert_eq!(
            up.hits(),
            1,
            "8 个并发读同一键只应打 1 次上游，实际 {}",
            up.hits()
        );
    }

    /// 日志字段：币安格式的正文拆出错误码与说明。
    #[test]
    fn error_fields_extract_binance_code_and_msg() {
        let (code, msg) =
            binance_error_fields(r#"{"code":-1003,"msg":"Way too many requests; IP banned"}"#);
        assert_eq!(code, Some(-1003));
        assert!(msg.contains("banned"), "{msg}");
    }

    /// 非币安格式（网关错误页）保留原文，但必须截断，不能整页刷进日志。
    #[test]
    fn error_fields_truncate_non_json_bodies() {
        let page = "<html>".repeat(1_000);
        let (code, msg) = binance_error_fields(&page);
        assert_eq!(code, None);
        assert_eq!(msg.chars().count(), LOG_BODY_MAX_CHARS);
    }

    /// 失败路径改走 `upstream_failure` 之后，限流仍然要进入共享冷却，
    /// 且返回的错误仍然带着真实等待时间。
    #[tokio::test]
    async fn upstream_failure_still_arms_cooldown() {
        let up = FakeUpstream::always(418, Some("1234"), r#"{"code":-1003,"msg":"banned"}"#).await;
        let c = up.client();
        let e = c
            .get_public_cached("/fapi/v1/depth", "symbol=ETHUSDC", 0)
            .await
            .expect_err("418 应返回错误");
        assert!(
            matches!(
                e,
                ExchangeError::RateLimited {
                    retry_after_ms: 1_234_000
                }
            ),
            "{e:?}"
        );
        assert!(c.cooldown_remaining_ms().is_some(), "418 后应进入冷却");
    }

    use rust_decimal_macros::dec;
}
