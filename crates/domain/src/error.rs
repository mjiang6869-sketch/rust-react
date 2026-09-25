//! 领域错误与交易所错误。
//!
//! `ExchangeError` 按**可恢复性**分类，而不是按消息文本。这是刻意的：
//! "这次失败能不能重试"必须是一个类型问题，而不是调用方读错误字符串猜测。
//! 旧实现用 `anyhow::Error` 包裹一切，导致超时和拒单无法区分，只能一律
//! 保守地 disarm（见旧 `main.rs` 的 live supervisor：所有失败路径都是
//! "解除武装并记日志"），一次网络抖动就变成需要人工介入的交易停机。

use rust_decimal::Decimal;
use thiserror::Error;

/// 域内校验错误。全部是可编程处理的，不含自由文本。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    #[error("价格必须为正数，收到 {0}")]
    NonPositivePrice(Decimal),
    #[error("数量必须为正数，收到 {0}")]
    NonPositiveQuantity(Decimal),
    #[error("数量 {raw} 按 step {step} 向下取整后为零")]
    QuantityBelowStep { raw: Decimal, step: Decimal },
    #[error("价格 {raw} 按 tick {tick} 取整后为零")]
    QuantizedToZero { raw: Decimal, tick: Decimal },
    #[error("tick_size 必须为正数，收到 {0}")]
    InvalidTickSize(Decimal),
    #[error("step_size 必须为正数，收到 {0}")]
    InvalidStepSize(Decimal),
    #[error("数量 {qty} 低于最小数量 {min}")]
    BelowMinQty { qty: Decimal, min: Decimal },
    #[error("名义价值 {notional} 低于最小值 {min}")]
    BelowMinNotional { notional: Decimal, min: Decimal },
    #[error("止损距离超过风控上限：{0}")]
    StopTooWide(String),
    #[error("订单引用了未知的持仓或保护单：{0}")]
    UnknownReference(String),
    #[error("非法状态迁移：{0}")]
    IllegalTransition(String),
}

/// 币安拒单原因。需要区分具体情形，因为处理方式完全不同。
#[derive(Debug, Clone, PartialEq, Eq, Error, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RejectReason {
    /// 币安错误码 5022：post-only 单会立即吃单，被拒绝。
    ///
    /// 关键特性（官方文档确认）：这类订单**不记入订单历史**，也**不推送
    /// WebSocket 事件**。也就是说下完单后无法通过查询确认它是否存在过。
    /// 因为 GTX 是做市唯一允许的下单方式，这条路径是常态而非异常。
    #[error("post-only 会立即成交，已被交易所拒绝（5022）")]
    PostOnlyWouldCross,
    #[error("保证金不足")]
    InsufficientMargin,
    #[error("价格超出交易所允许区间")]
    PriceOutOfRange,
    #[error("数量不满足交易所过滤器")]
    InvalidQuantity,
    #[error("合约已停止交易")]
    InstrumentNotTrading,
    #[error("交易所拒单：{0}")]
    Other(String),
}

/// 交易所调用失败。分类依据是**能否安全重试**。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExchangeError {
    /// 请求确定未被交易所接收（连接建立失败、请求构造失败）。
    /// 可以安全重发——但要先确认订单不在本地状态机里。
    #[error("请求未送达，可安全重试：{0}")]
    Definitive(String),

    /// 交易所明确拒绝。**禁止重试**——重试只会再被拒一次。
    #[error("订单被拒绝：{0}")]
    Rejected(#[from] RejectReason),

    /// 状态未知：超时、5xx、连接中断后无法判断请求是否送达。
    ///
    /// **这是本系统最重要的一类错误。** 它必须把订单推进到
    /// `OrderState::Unknown` 并触发查询对账，**绝不能直接重试提交**。
    /// 旧实现无法表达这个区分，只能一律 disarm。
    #[error("订单状态未知，必须先查询对账：{0}")]
    Unknown(String),

    #[error("交易所限流，建议 {retry_after_ms} 毫秒后重试")]
    RateLimited { retry_after_ms: u64 },

    /// 致命错误：凭据无效、账户被限制、端点不可达。停止交易并告警。
    #[error("致命错误，应停止交易：{0}")]
    Fatal(String),
}

impl ExchangeError {
    /// 是否允许在**未对账**的前提下直接重试。
    ///
    /// 只有 `Definitive` 为真。`Unknown` 明确为假——那是需要查询的路径。
    pub fn retryable_without_reconcile(&self) -> bool {
        matches!(self, ExchangeError::Definitive(_))
    }

    /// 是否应该停止交易。
    pub fn is_fatal(&self) -> bool {
        matches!(self, ExchangeError::Fatal(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这个分类决定了实盘的安全行为，必须锁住。
    #[test]
    fn only_definitive_errors_are_retryable_without_reconcile() {
        assert!(ExchangeError::Definitive("连接被拒".into()).retryable_without_reconcile());

        let unknown = ExchangeError::Unknown("请求超时".into());
        assert!(
            !unknown.retryable_without_reconcile(),
            "状态未知时必须先查询，不能直接重发——否则可能重复下单"
        );

        let rejected = ExchangeError::Rejected(RejectReason::PostOnlyWouldCross);
        assert!(!rejected.retryable_without_reconcile());
    }

    #[test]
    fn fatal_errors_are_flagged_for_halt() {
        assert!(ExchangeError::Fatal("签名无效".into()).is_fatal());
        assert!(
            !ExchangeError::RateLimited {
                retry_after_ms: 1000
            }
            .is_fatal()
        );
    }
}
