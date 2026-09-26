//! 上游限流的冷却状态与 `Retry-After` 解析。
//!
//! # 为什么需要这个模块
//!
//! 币安在超限时返回 429，**继续打**就会升级成 418 封禁；418 的
//! `Retry-After` 是解封剩余秒数（例如 1234 秒 ≈ 20 分钟）。之前这里
//! 把响应头丢掉了，冷却时间被写死成 1 秒，于是：
//!
//! 1. 客户端以为 1 秒后就能重试，实际还要等 20 分钟；
//! 2. 每个请求方各自"发现"封禁——浏览器开两个标签就有两倍请求量。
//!
//! 所以冷却必须是**进程内共享的**：谁先撞上限流，其他人就一起等。
//! 这不是优化，是正确性——封禁是记在 IP 上的，不区分是哪一次请求触发的。
//!
//! # 为什么不在这里 sleep
//!
//! 本模块只记录"什么时候可以再发"，不阻塞调用方。等待由调用方（前端退避、
//! 后台任务）决定。在这里 sleep 会占住 tokio 工作线程，而且签名请求的
//! 超时语义（见 `client.rs`）会被打乱。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;

/// `Retry-After` 缺失时的保守兜底：1 秒。
///
/// 取值理由：这是币安文档里最短的限流等待量级。宁可多等一点，也不要
/// 在封禁期内继续打——那会把 2 分钟的封禁拖成 3 天。
pub const DEFAULT_RETRY_AFTER_MS: u64 = 1_000;

/// `retry_after_ms` 的上限：1 小时。
///
/// 币安最长的封禁是 3 天，但单一响应头给出那么大的值通常意味着解析出错或
/// 上游返回了异常内容。1 小时足够覆盖正常的 429/418，超过则视为异常并
/// 按上限截断——不会因为一个坏响应头让服务停摆一整天。
pub const MAX_RETRY_AFTER_MS: u64 = 60 * 60 * 1_000;

/// 解析 `Retry-After` 响应头（秒）。
///
/// # 只接受秒数
///
/// HTTP 允许 `Retry-After` 用 HTTP-date 表示，但币安只发秒数。同时支持
/// 两种格式会引入"日期解析失败该退化成什么"的模糊地带，所以这里刻意
/// 只认整数秒——不认识的格式退化为兜底值，不会静默算出离谱的等待时间。
///
/// 缺失、非数字、0 都退化为 [`DEFAULT_RETRY_AFTER_MS`]：一个不存在或为 0
/// 的等待时间不能解释成"立刻重试"。
pub fn parse_retry_after(raw: Option<&str>) -> u64 {
    parse_retry_after_opt(raw).unwrap_or(DEFAULT_RETRY_AFTER_MS)
}

/// 解析 `Retry-After`，但**保留"解析失败"与"确实没有"的区分**。
///
/// 调用方需要这个区分来安排兜底顺序：头解析不出来时还要去正文里找
/// `banned until`，而直接拿 [`parse_retry_after`] 的兜底值就会跳过那一步。
pub fn parse_retry_after_opt(raw: Option<&str>) -> Option<u64> {
    let secs = raw?.trim().parse::<u64>().ok()?;
    if secs == 0 {
        return None;
    }
    Some(secs.saturating_mul(1_000).min(MAX_RETRY_AFTER_MS))
}

/// 从错误正文里的 `banned until <毫秒时间戳>` 提取剩余等待时间。
///
/// # 为什么需要它
///
/// 418 的正文长这样：
///
/// ```text
/// {"code":-1003,"msg":"Way too much request weight used; IP banned until 1790354159999."}
/// ```
///
/// `Retry-After` 正常都会有，但**它缺失时正文里的解封时间是唯一的真相**。
/// 这种情况比看起来常见：中间有代理、或响应头被中间层改写。
///
/// 返回 `None` 表示正文里没有可解析的解封时间——调用方应退化为
/// [`DEFAULT_RETRY_AFTER_MS`]，而不是假设"没封禁"。
pub fn parse_banned_until_ms(body: &str, now_ms: i64) -> Option<u64> {
    const MARKER: &str = "banned until ";
    let start = body.find(MARKER)? + MARKER.len();
    let rest = body.get(start..)?;

    // 只吃到第一个非数字字符：时间戳后面可能跟 `.` 或 `"`。
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let until_ms: i64 = digits.parse().ok()?;

    // 已经解封或时间戳明显不合理（早于现在）→ 没有剩余等待时间
    let remaining = until_ms.checked_sub(now_ms)?;
    if remaining <= 0 {
        return None;
    }
    u64::try_from(remaining)
        .ok()
        .map(|ms| ms.min(MAX_RETRY_AFTER_MS))
}

/// 进程内共享的冷却截止时刻。
///
/// `Clone` 只复制 `Arc`，所以多个克隆看到的是**同一份**冷却状态。这一点
/// 是设计前提：`BinanceClient` 会被多个请求方共用，如果每个实例各存一份
/// 冷却时间，封禁期间仍然会被打出多倍请求量。
///
/// 内部是 `AtomicU64`（毫秒时间戳）而不是 `Mutex<Option<Instant>>`：读多写
/// 极少，且**不能 await**——冷却检查发生在发送请求的热路径上。
#[derive(Clone, Debug, Default)]
pub struct Cooldown {
    until_ms: Arc<AtomicU64>,
}

impl Cooldown {
    /// 无冷却状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前时刻的毫秒时间戳。
    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    /// 剩余冷却时间。`None` 表示当前可以发请求。
    pub fn remaining_ms(&self) -> Option<u64> {
        self.remaining_at(Self::now_ms())
    }

    /// 以给定时刻计算剩余冷却。抽出来是为了让测试不必真的等待。
    fn remaining_at(&self, now_ms: i64) -> Option<u64> {
        let until = self.until_ms.load(Ordering::Relaxed) as i64;
        let remaining = until.checked_sub(now_ms)?;
        if remaining <= 0 {
            return None;
        }
        u64::try_from(remaining).ok()
    }

    /// 是否处于冷却中。
    pub fn is_cooling_down(&self) -> bool {
        self.remaining_ms().is_some()
    }

    /// 施加冷却，从**现在**开始计时。
    ///
    /// 用 `fetch_max` 而不是 `store`：并发的多个失败响应可能带着不同的等待
    /// 时间（例如一个 429 说 30 秒、紧跟一个 418 说 1200 秒）。取最大值才是
    /// 正确行为——较短的等待时间不能把较长的封禁覆盖掉。
    pub fn arm_ms(&self, duration_ms: u64) {
        self.arm_until(Self::now_ms().saturating_add(duration_ms as i64));
    }

    fn arm_until(&self, until_ms: i64) {
        let until = u64::try_from(until_ms.max(0)).unwrap_or(0);
        self.until_ms.fetch_max(until, Ordering::Relaxed);
    }

    /// 采用另一个冷却实例的状态（取两者较晚的截止时刻）。
    ///
    /// 用于把同一进程里的多个客户端统一到一份冷却上。取最大值而不是覆盖：
    /// 后加入的客户端不该把一个更长的封禁缩短。
    pub fn adopt(&self, other: &Cooldown) {
        let until = other.until_ms.load(Ordering::Relaxed);
        self.until_ms.fetch_max(until, Ordering::Relaxed);
    }

    /// 记录一次成功。
    ///
    /// **只在冷却已经到期时**才清零。否则一个早先发出的请求迟到的成功响应
    /// 会把仍在生效的封禁抹掉——那正是"封禁期内继续打"的成因。
    pub fn clear_if_elapsed(&self) {
        let now = Self::now_ms();
        let until = self.until_ms.load(Ordering::Relaxed) as i64;
        if until <= now {
            self.until_ms.store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实的 418 响应头。参考事故里是 1234 秒。
    #[test]
    fn retry_after_is_read_as_seconds() {
        assert_eq!(parse_retry_after(Some("1234")), 1_234_000);
        assert_eq!(parse_retry_after(Some("1")), 1_000);
        assert_eq!(parse_retry_after(Some("  60 ")), 60_000, "容忍空白");
    }

    /// 缺失或异常的响应头**不能**解释成"立刻重试"。
    #[test]
    fn missing_or_invalid_retry_after_falls_back_conservatively() {
        for raw in [
            None,
            Some(""),
            Some("0"),
            Some("abc"),
            Some("-5"),
            Some("Wed, 21 Oct 2026 07:28:00 GMT"),
        ] {
            assert_eq!(
                parse_retry_after(raw),
                DEFAULT_RETRY_AFTER_MS,
                "{raw:?} 应退化为兜底值而非 0"
            );
        }
    }

    /// 离谱的响应头要截断，不能让服务停摆一整天。
    #[test]
    fn absurd_retry_after_is_capped() {
        assert_eq!(parse_retry_after(Some("99999999")), MAX_RETRY_AFTER_MS);
        assert_eq!(
            parse_retry_after(Some("18446744073709551615")),
            MAX_RETRY_AFTER_MS
        );
    }

    /// 正文里的 `banned until` 是响应头缺失时的唯一真相。
    ///
    /// 观察时刻由解封时刻倒推，不写死——两个字面量手算极易错位，
    /// 而错位会退化成"剩余时间为负"从而被静默当成 None。
    #[test]
    fn banned_until_is_extracted_from_body() {
        const UNTIL: i64 = 1_790_354_159_999;
        let body = format!(
            r#"{{"code":-1003,"msg":"Way too much request weight used; IP banned until {UNTIL}."}}"#
        );
        // 距解封 60 秒
        assert_eq!(parse_banned_until_ms(&body, UNTIL - 60_000), Some(60_000));
        // 距解封 20 分钟——事故里 `Retry-After: 1234` 的量级
        assert_eq!(
            parse_banned_until_ms(&body, UNTIL - 1_234_000),
            Some(1_234_000)
        );
    }

    #[test]
    fn banned_until_handles_absent_or_past_timestamps() {
        assert_eq!(
            parse_banned_until_ms(r#"{"code":-1003,"msg":"nope"}"#, 0),
            None
        );
        let body = r#"{"msg":"IP banned until 1000."}"#;
        assert_eq!(
            parse_banned_until_ms(body, 5_000),
            None,
            "已过期的解封时间不产生等待"
        );
        assert_eq!(parse_banned_until_ms("banned until abc.", 0), None);
        assert_eq!(parse_banned_until_ms("banned until .", 0), None);
    }

    #[test]
    fn banned_until_is_never_negative_or_overflowing() {
        let body = r#"{"msg":"IP banned until 99999999999999999."}"#;
        assert_eq!(parse_banned_until_ms(body, 0), Some(MAX_RETRY_AFTER_MS));
    }

    /// 测试用基准时刻。用相对时间而不是字面量：`arm_until` 的入参是
    /// 毫秒时间戳，写死的小数字会因为早于当前时间而被判为"已过期"。
    fn base() -> i64 {
        1_790_000_000_000
    }

    #[test]
    fn fresh_cooldown_is_inactive() {
        let c = Cooldown::new();
        assert_eq!(c.remaining_at(base()), None);
    }

    #[test]
    fn armed_cooldown_reports_remaining_time() {
        let c = Cooldown::new();
        c.arm_until(base() + 1_000_000);
        assert_eq!(c.remaining_at(base() + 400_000), Some(600_000));
    }

    /// 用真实时钟验证 `remaining_ms`——上面的相对时刻测试绕过了它。
    #[test]
    fn arm_ms_sets_a_real_deadline_in_the_future() {
        let c = Cooldown::new();
        assert!(!c.is_cooling_down());
        c.arm_ms(60_000);
        assert!(
            c.is_cooling_down(),
            "arm_ms 之后必须处于冷却中，否则封禁期会继续打上游"
        );
        let remaining = c.remaining_ms().expect("应有剩余时间");
        assert!(
            (1..=60_000).contains(&remaining),
            "剩余时间应在 60 秒内：{remaining}"
        );
    }

    /// 0 秒冷却等于没冷却——调用方不该用它表示"无限等待"或"立刻可发"。
    #[test]
    fn zero_duration_is_not_cooling_down() {
        let c = Cooldown::new();
        c.arm_ms(0);
        assert!(!c.is_cooling_down());
    }

    #[test]
    fn elapsed_cooldown_reports_none() {
        let c = Cooldown::new();
        c.arm_until(base() + 1_000_000);
        assert_eq!(
            c.remaining_at(base() + 1_000_000),
            None,
            "边界：正好到期即可发送"
        );
        assert_eq!(c.remaining_at(base() + 2_000_000), None);
    }

    /// 较短的等待时间不能覆盖较长的封禁。
    #[test]
    fn shorter_cooldown_does_not_shorten_a_longer_one() {
        let c = Cooldown::new();
        c.arm_until(base() + 10_000_000);
        c.arm_until(base() + 2_000_000);
        assert_eq!(
            c.remaining_at(base() + 1_000_000),
            Some(9_000_000),
            "后到的短冷却不应把长封禁截短"
        );
    }

    #[test]
    fn longer_cooldown_extends() {
        let c = Cooldown::new();
        c.arm_until(base() + 2_000_000);
        c.arm_until(base() + 10_000_000);
        assert_eq!(c.remaining_at(base() + 1_000_000), Some(9_000_000));
    }

    /// 迟到的成功响应不能抹掉仍在生效的封禁。
    #[test]
    fn clear_only_applies_after_expiry() {
        let c = Cooldown::new();
        // 仍在生效：clear 必须无效
        c.arm_ms(60_000);
        c.clear_if_elapsed();
        assert!(c.is_cooling_down(), "未到期的冷却不能被一次成功响应清掉");

        // 已过期：clear 应清零
        let e = Cooldown::new();
        e.arm_until(base() - 5_000);
        e.clear_if_elapsed();
        assert_eq!(e.until_ms.load(Ordering::Relaxed), 0);
    }

    /// 克隆共享同一份状态——这是"封禁期间不打上游"的前提。
    #[test]
    fn clones_share_state() {
        let a = Cooldown::new();
        let b = a.clone();
        a.arm_ms(60_000);
        assert!(
            b.is_cooling_down(),
            "克隆必须看到同一份冷却，否则每个请求方会各自撞一次限流"
        );
    }

    /// `adopt` 用于把多个客户端统一到一份冷却上，且不能缩短已有的封禁。
    #[test]
    fn adopt_takes_the_later_deadline() {
        let target = Cooldown::new();
        let source = Cooldown::new();
        source.arm_until(base() + 10_000_000);
        target.arm_until(base() + 1_000_000);
        target.adopt(&source);
        assert_eq!(
            target.remaining_at(base()),
            Some(10_000_000),
            "应取较晚的截止时刻"
        );

        // 反向：来源更早时不应把目标缩短
        let short = Cooldown::new();
        short.arm_until(base() + 1_000);
        target.adopt(&short);
        assert_eq!(target.remaining_at(base()), Some(10_000_000));
    }
}
