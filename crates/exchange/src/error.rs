//! 交易所层的错误类型。
//!
//! 复用 `domain::ExchangeError`——它的"按可恢复性分类"设计（Definitive /
//! Rejected / Unknown / RateLimited / Fatal）已经满足本层需要，再定义一套
//! 只会造成两套错误类型互相转换的负担。
//!
//! 签名与 endpoint 白名单的错误在这里映射进去：白名单失败是**致命错误**
//! （配置错误，不该重试），时间戳异常同理。

pub use domain::ExchangeError;

use crate::signing::SignError;

impl From<SignError> for ExchangeError {
    fn from(e: SignError) -> Self {
        // 三类签名错误都是配置或环境问题，重试不会好转，所以归为 Fatal。
        ExchangeError::Fatal(e.to_string())
    }
}
