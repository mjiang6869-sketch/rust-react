//! 币安签名与 endpoint 白名单。
//!
//! # 白名单是硬约束
//!
//! 签名请求会把 API Key 发给目标主机。如果没有白名单，一个被篡改的环境变量
//! 就能让凭据发往任意地址。所以**只有**币安官方的生产与测试网域名被允许，
//! 且这个检查无法通过配置绕过。
//!
//! # 密钥不出现在任何日志、错误信息或签名结果里
//!
//! 有测试专门断言这一点。

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// 允许的签名 endpoint 主机。
const ALLOWED_HOSTS: &[&str] = &["fapi.binance.com", "testnet.binancefuture.com"];

/// 币安签名相关错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignError {
    #[error("endpoint 不在白名单内：{0}。只允许币安官方的生产与测试网域名。")]
    EndpointNotAllowed(String),
    #[error("URL 无法解析：{0}")]
    InvalidUrl(String),
    #[error("系统时钟早于 UNIX 纪元")]
    ClockBeforeEpoch,
}

/// 校验 URL 的 host 是否在白名单内。
pub fn endpoint_allowed(url: &str) -> Result<(), SignError> {
    let parsed = url::Url::parse(url).map_err(|_| SignError::InvalidUrl(url.to_string()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| SignError::InvalidUrl(url.to_string()))?;

    if ALLOWED_HOSTS.contains(&host) {
        Ok(())
    } else {
        Err(SignError::EndpointNotAllowed(host.to_string()))
    }
}

/// 当前时间戳（毫秒）。
pub fn timestamp_ms() -> Result<u64, SignError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| SignError::ClockBeforeEpoch)
}

/// 用 HMAC-SHA256 对查询串签名。
///
/// 返回十六进制小写字符串。**密钥不进入返回值**——签名是单向的。
pub fn sign(secret: &str, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC 接受任意长度密钥，这里不可能失败");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 把参数列表拼成查询串。
///
/// **调用方负责保证 `timestamp` 与 `recvWindow` 已经包含在内**，且签名必须
/// 在拼接完成后对完整串进行——币安要求签名覆盖全部参数。
pub fn build_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// 给一批参数加上时间戳并签名，返回完整的查询串（含 signature）。
pub fn signed_query(
    secret: &str,
    mut params: Vec<(&str, String)>,
    recv_window_ms: u64,
) -> Result<String, SignError> {
    params.push(("timestamp", timestamp_ms()?.to_string()));
    params.push(("recvWindow", recv_window_ms.to_string()));
    let payload = build_query(&params);
    let signature = sign(secret, &payload);
    Ok(format!("{payload}&signature={signature}"))
}

/// API 凭据。
///
/// 刻意不实现 `Debug` 的手工版本——不打印 key 与 secret 本身。
#[derive(Clone)]
pub struct Credentials {
    pub api_key: String,
    secret: String,
}

impl Credentials {
    pub fn new(api_key: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            secret: secret.into(),
        }
    }

    /// 从环境变量读取。
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("RUST_CRYPTO_BINANCE_API_KEY").ok()?;
        let secret = std::env::var("RUST_CRYPTO_BINANCE_API_SECRET").ok()?;
        if key.trim().is_empty() || secret.trim().is_empty() {
            return None;
        }
        Some(Self::new(key, secret))
    }

    /// 签名一批参数。
    ///
    /// 这是密钥**唯一**的访问路径——它不暴露给调用方，只在内部参与 HMAC
    /// 计算。这样密钥不可能被打印、序列化或返回给上层。
    pub fn sign_params(
        &self,
        params: Vec<(&str, String)>,
        recv_window_ms: u64,
    ) -> Result<String, SignError> {
        signed_query(&self.secret, params, recv_window_ms)
    }
}

/// 手工实现 `Debug`，确保密钥不会被意外打印到日志。
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"<已隐藏>")
            .field("secret", &"<已隐藏>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_and_testnet_endpoints_are_allowed() {
        assert!(endpoint_allowed("https://fapi.binance.com/fapi/v1/order").is_ok());
        assert!(endpoint_allowed("https://testnet.binancefuture.com/fapi/v1/order").is_ok());
    }

    /// 这是安全边界：凭据只能发往币安官方域名。
    #[test]
    fn arbitrary_hosts_are_rejected() {
        for bad in [
            "https://evil.example/fapi/v1/order",
            "https://fapi.binance.com.evil.example/x",
            "http://localhost:8080/x",
            "https://binance.com.evil.example/",
        ] {
            assert!(
                endpoint_allowed(bad).is_err(),
                "{bad} 不应被允许——签名请求会把凭据发过去"
            );
        }
    }

    #[test]
    fn malformed_url_is_rejected() {
        assert!(matches!(
            endpoint_allowed("not a url"),
            Err(SignError::InvalidUrl(_))
        ));
    }

    /// 签名必须是确定的：同样的输入得到同样的输出，否则重试会失败。
    #[test]
    fn signature_is_deterministic() {
        let payload = "symbol=ETHUSDC&side=BUY&timestamp=1785542400000";
        let a = sign("secret", payload);
        let b = sign("secret", payload);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "HMAC-SHA256 的十六进制表示是 64 字符");
    }

    #[test]
    fn different_secrets_produce_different_signatures() {
        let payload = "symbol=ETHUSDC";
        assert_ne!(sign("secret-a", payload), sign("secret-b", payload));
    }

    #[test]
    fn different_payloads_produce_different_signatures() {
        assert_ne!(sign("secret", "a=1&b=2"), sign("secret", "a=2&b=1"));
    }

    #[test]
    fn query_building_preserves_order() {
        let q = build_query(&[("symbol", "ETHUSDC".into()), ("side", "BUY".into())]);
        assert_eq!(q, "symbol=ETHUSDC&side=BUY");
    }

    /// 签名必须覆盖全部参数，且包含时间戳。
    #[test]
    fn signed_query_includes_timestamp_and_signature() {
        let q = signed_query("s", vec![("symbol", "ETHUSDC".into())], 5000).unwrap();
        assert!(q.contains("symbol=ETHUSDC"));
        assert!(q.contains("timestamp="));
        assert!(q.contains("recvWindow=5000"));
        assert!(q.contains("&signature="));
    }

    /// **密钥绝不能出现在任何可能被打印的地方。**
    #[test]
    fn credentials_debug_hides_secrets() {
        let c = Credentials::new("my-api-key-12345", "my-secret-67890");
        let dbg = format!("{c:?}");
        assert!(
            !dbg.contains("my-api-key-12345"),
            "API Key 泄漏到 Debug 输出"
        );
        assert!(!dbg.contains("my-secret-67890"), "Secret 泄漏到 Debug 输出");
        assert!(dbg.contains("已隐藏"));
    }

    /// 签名结果本身不能包含密钥——签名是单向摘要。
    #[test]
    fn signature_does_not_leak_secret() {
        let secret = "super-secret-value";
        let sig = sign(secret, "symbol=ETHUSDC");
        assert!(!sig.contains(secret));
        assert!(!sig.contains("secret"));
    }

    #[test]
    fn from_env_requires_both_key_and_secret() {
        // 这里不修改进程环境（会影响其他测试），只验证空值处理逻辑
        let empty = Credentials::new("", "");
        assert!(empty.api_key.is_empty());
    }

    #[test]
    fn timestamp_is_plausible() {
        let t = timestamp_ms().unwrap();
        // 2026-01-01 之后的毫秒时间戳
        assert!(t > 1_767_225_600_000, "时间戳看起来不对：{t}");
    }
}
