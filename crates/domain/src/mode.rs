//! 运行模式。
//!
//! # 为什么这是一个显式类型而不是布尔值
//!
//! 旧实现用 `bool`（或字符串）表达"是否实盘"，于是出现了"默认值让服务从
//! 模拟盘无提示切到主网"的风险。这里把它做成枚举，并且：
//!
//! 1. **默认值是 `Paper`**，且不实现 `From<bool>` 之类的隐式转换
//! 2. 从字符串解析时**只接受精确匹配**，不做模糊匹配
//! 3. 实盘的启用需要显式构造，且必须伴随 ARM 状态（见 `LiveSafety`）
//!
//! 关键约束：模拟盘与实盘的差异不只是"发不发单"，还包括可用数据源、
//! 费率来源、账户对账方式。把它们收在一个枚举里，让所有分支都必须显式
//! 处理两种情形。

use serde::{Deserialize, Serialize};

/// 服务模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServiceMode {
    /// 模拟盘。本地撮合，不接触交易所。
    #[default]
    Paper,
    /// 实盘。连接交易所，可提交真实订单。
    Live,
}

impl ServiceMode {
    /// 是否是实盘。
    pub const fn is_live(self) -> bool {
        matches!(self, ServiceMode::Live)
    }

    /// 是否允许提交真实订单。
    pub const fn allows_real_orders(self) -> bool {
        matches!(self, ServiceMode::Live)
    }

    /// 从环境变量或配置字符串解析。
    ///
    /// **只接受精确匹配**。`"live"`、`"LIVE"`、`"Live"` 都可（大小写不敏感），
    /// 但 `"livex"`、`"true"`、`"1"` 一律报错——模糊匹配是"无提示切到主网"
    /// 这类事故的温床。
    pub fn parse(s: &str) -> Result<Self, ModeParseError> {
        let trimmed = s.trim();
        match trimmed.to_ascii_uppercase().as_str() {
            "PAPER" => Ok(ServiceMode::Paper),
            "LIVE" => Ok(ServiceMode::Live),
            // 错误里回显**用户原始输入**而非大写化后的版本——
            // 报 `LIVEX` 会让人怀疑自己到底输了什么。
            _ => Err(ModeParseError {
                input: trimmed.to_string(),
            }),
        }
    }

    /// 面向用户的中文名称。
    pub const fn label(self) -> &'static str {
        match self {
            ServiceMode::Paper => "模拟盘",
            ServiceMode::Live => "实盘",
        }
    }
}

/// 模式解析错误。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("无法识别的模式「{input}」：只接受 PAPER 或 LIVE（大小写不敏感）")]
pub struct ModeParseError {
    pub input: String,
}

/// 实盘安全闸门。
///
/// # 为什么需要三个独立条件
///
/// 实盘交易的前提不是"模式设为 LIVE"，而是三个必须同时成立的条件：
///
/// 1. **用户数据流已连接** —— 否则收不到成交回报，无法知道订单真实状态
/// 2. **账户已对账** —— 本地持仓与交易所一致，否则可能重复开仓或漏平
/// 3. **显式 ARM** —— 操作者主动开启，且**不会因重连或重启自动恢复**
///
/// 第 3 条尤其重要：任何异常都只能向更安全的状态切换，不能自动回到
/// 可交易状态。旧实现把这套逻辑散在 `main.rs` 的 supervisor 里，导致
/// 一次网络抖动就 disarm 且需要人工恢复，而重启后又可能忘记重新检查。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSafety {
    user_stream_connected: bool,
    account_reconciled: bool,
    armed: bool,
}

impl LiveSafety {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_user_stream_connected(&mut self, connected: bool) {
        self.user_stream_connected = connected;
        if !connected {
            // 断线即解除武装——收不到成交回报时继续挂单等于盲开。
            self.armed = false;
        }
    }

    pub fn mark_account_reconciled(&mut self, reconciled: bool) {
        self.account_reconciled = reconciled;
        if !reconciled {
            // 对账不一致时必须停止交易，而不是继续。
            self.armed = false;
        }
    }

    /// 主动开启交易。
    ///
    /// **仅当两个前置条件都满足时才生效**——返回 `false` 表示开启失败，
    /// 调用方应把原因告知用户（缺哪一项）。
    pub fn arm(&mut self) -> bool {
        if self.user_stream_connected && self.account_reconciled {
            self.armed = true;
            true
        } else {
            false
        }
    }

    /// 解除武装。任何时候都可以调用。
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// 是否允许提交订单。
    pub fn can_submit(&self) -> bool {
        self.user_stream_connected && self.account_reconciled && self.armed
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

    /// 当前阻止交易的原因（用于界面提示）。
    pub fn blocking_reasons(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if !self.user_stream_connected {
            v.push("用户数据流未连接——收不到成交回报");
        }
        if !self.account_reconciled {
            v.push("账户未对账——本地持仓可能与交易所不一致");
        }
        if !self.armed {
            v.push("未开启交易（ARM）");
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_paper() {
        assert_eq!(ServiceMode::default(), ServiceMode::Paper);
        assert!(!ServiceMode::default().allows_real_orders());
    }

    /// 只接受精确匹配——模糊匹配会导致"无提示切到主网"。
    #[test]
    fn mode_parsing_is_strict() {
        assert_eq!(ServiceMode::parse("PAPER").unwrap(), ServiceMode::Paper);
        assert_eq!(ServiceMode::parse("paper").unwrap(), ServiceMode::Paper);
        assert_eq!(ServiceMode::parse(" Live ").unwrap(), ServiceMode::Live);

        for bad in ["livex", "true", "1", "yes", "real", "", "pap er"] {
            assert!(ServiceMode::parse(bad).is_err(), "「{bad}」不应被接受");
        }
    }

    #[test]
    fn parse_error_names_the_offending_input() {
        let e = ServiceMode::parse("livex").unwrap_err();
        assert!(e.to_string().contains("livex"));
        assert!(e.to_string().contains("PAPER"));
    }

    #[test]
    fn live_mode_allows_real_orders() {
        assert!(ServiceMode::Live.allows_real_orders());
        assert!(ServiceMode::Live.is_live());
        assert!(!ServiceMode::Paper.is_live());
    }

    /// 三个条件必须同时满足才能下单。
    #[test]
    fn all_three_conditions_are_required_before_submitting() {
        let mut s = LiveSafety::new();
        assert!(!s.can_submit(), "初始状态不能下单");

        assert!(!s.arm(), "前置条件未满足时 arm 应失败");

        s.mark_user_stream_connected(true);
        assert!(!s.arm(), "仅数据流连接还不够");
        assert!(!s.can_submit());

        s.mark_account_reconciled(true);
        assert!(s.arm(), "两个前置条件满足后可以开启");
        assert!(s.can_submit());
    }

    /// **数据流断线必须自动解除武装。**
    /// 收不到成交回报时继续挂单等于盲开。
    #[test]
    fn stream_disconnect_disarms_automatically() {
        let mut s = LiveSafety::new();
        s.mark_user_stream_connected(true);
        s.mark_account_reconciled(true);
        s.arm();
        assert!(s.can_submit());

        s.mark_user_stream_connected(false);
        assert!(!s.is_armed(), "断线必须自动解除武装");
        assert!(!s.can_submit());
    }

    /// **对账不一致必须自动解除武装。**
    #[test]
    fn reconciliation_mismatch_disarms_automatically() {
        let mut s = LiveSafety::new();
        s.mark_user_stream_connected(true);
        s.mark_account_reconciled(true);
        s.arm();

        s.mark_account_reconciled(false);
        assert!(!s.is_armed(), "对账不一致必须自动解除武装");
    }

    /// **重启后不能自动回到可交易状态。**
    ///
    /// 默认构造的 `LiveSafety` 必须是未武装的——旧实现里重启后可能忘记
    /// 重新检查，导致在没有对账的情况下恢复交易。
    #[test]
    fn restarting_does_not_restore_armed_state() {
        let s = LiveSafety::new();
        assert!(!s.is_armed());
        assert!(!s.can_submit());
        assert_eq!(s.blocking_reasons().len(), 3, "三项原因都要能列出");
    }

    #[test]
    fn blocking_reasons_are_specific() {
        let mut s = LiveSafety::new();
        s.mark_user_stream_connected(true);
        s.mark_account_reconciled(true);
        let reasons = s.blocking_reasons();
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("ARM"), "应明确提示缺 ARM：{reasons:?}");

        s.arm();
        assert!(s.blocking_reasons().is_empty());
    }

    #[test]
    fn disarm_works_at_any_time() {
        let mut s = LiveSafety::new();
        s.mark_user_stream_connected(true);
        s.mark_account_reconciled(true);
        s.arm();
        s.disarm();
        assert!(!s.can_submit());
        assert!(s.user_stream_connected(), "解除武装不改变连接状态");
        assert!(s.account_reconciled());
    }

    #[test]
    fn labels_are_user_facing_chinese() {
        assert_eq!(ServiceMode::Paper.label(), "模拟盘");
        assert_eq!(ServiceMode::Live.label(), "实盘");
    }
}
