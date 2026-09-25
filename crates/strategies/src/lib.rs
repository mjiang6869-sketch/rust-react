//! 策略实现。
//!
//! # 插件化的目的
//!
//! 策略只实现 `domain::Strategy`，产出意图，不做价格计算、不碰订单、不碰
//! 交易所。所有下单路径、量化、保护单推导、风控都由框架完成。所以新增策略
//! 不可能引入"第四份止盈公式"，也不可能绕过风控。
//!
//! # 加新策略的步骤
//!
//! 1. 新建一个文件，实现 `Strategy` trait。
//! 2. 在 `all()` 里注册。
//! 3. 补参数说明（`parameters()`），前端会自动渲染表单并解释每个参数。
//!
//! 不需要改任何其他模块。

pub mod range_maker;

pub use range_maker::{RangeMaker, RangeMakerLadder, RangeMakerParams, SideMode};

/// 按 ID 构造策略。CLI 与 API 用它。
pub fn by_id(id: &str) -> Option<Box<dyn domain::Strategy>> {
    match id {
        "range_maker" => Some(Box::new(RangeMaker::with_defaults())),
        "range_maker_ladder" => Some(Box::new(RangeMakerLadder::with_defaults())),
        _ => None,
    }
}

/// 所有已注册策略的 ID。
pub const STRATEGY_IDS: &[&str] = &["range_maker", "range_maker_ladder"];

/// 所有已注册策略的名称与说明，供前端列出可选项。
pub fn catalog() -> Vec<StrategyInfo> {
    STRATEGY_IDS
        .iter()
        .filter_map(|id| {
            by_id(id).map(|s| StrategyInfo {
                id: s.id().to_string(),
                name: s.name().to_string(),
                warmup_candles: s.warmup_candles(),
                parameters: s.parameters(),
            })
        })
        .collect()
}

/// 策略的展示信息。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StrategyInfo {
    pub id: String,
    pub name: String,
    /// 决策前需要的已收盘 K 线根数。
    pub warmup_candles: usize,
    pub parameters: Vec<domain::ParameterSpec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_declared_strategies_can_be_constructed() {
        for id in STRATEGY_IDS {
            assert!(by_id(id).is_some(), "策略 {id} 应能构造");
        }
        assert!(by_id("nonexistent").is_none());
    }

    /// 目录信息必须完整——前端靠它渲染策略选择与参数表单。
    #[test]
    fn catalog_exposes_names_and_parameters() {
        let cat = catalog();
        assert_eq!(cat.len(), STRATEGY_IDS.len());
        for s in &cat {
            assert!(!s.name.is_empty(), "策略 {} 缺少名称", s.id);
            assert!(s.warmup_candles > 0, "策略 {} 需要声明预热根数", s.id);
            assert!(!s.parameters.is_empty(), "策略 {} 缺少参数说明", s.id);
        }
    }
}
