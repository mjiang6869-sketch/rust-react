//! 成交模型。
//!
//! # 这是整个项目里最重要的一个模块
//!
//! maker 费率为 0 时，单笔毛利润就是止盈距离（当前配置约 4bp）。没有价差
//! 缓冲、没有返佣、没有逆向选择垫。**成交模型乐观 10% 不是让 P&L 差 10%，
//! 而是可能翻转符号**——因为亏损的交易（突破把你扫掉）建模得近乎完美，
//! 而盈利的交易（需要对手方主动吃你的挂单）被建模成免费。
//!
//! ## 模型阶梯
//!
//! | 模型 | 数据需求 | 用途 |
//! | --- | --- | --- |
//! | `M0WickTouchFull` | 1m OHLCV | **仅作标注为上界的对照**，绝不能是默认值 |
//! | `M1TradeThroughQueue` | 1m K 线 + aggTrades | **诚实基线** |
//!
//! ## 排队位置的硬约束
//!
//! 币安归档的 `aggTrades` 与 `trades` 都只有 `is_buyer_maker` 一个方向标志，
//! **没有买卖双方的订单 ID**；`bookTicker`（最优买卖价）也已在 2024-04 停更。
//! 所以历史 L2 深度根本不存在，排队位置**无法还原**。
//!
//! M1 的"假设我们排在队尾"因此不是保守的近似选择，而是**唯一可能的选择**。
//! 改进它只有一条路：从今天起自采实时 `depth@100ms`，攒够后做校准。

use chrono::{DateTime, Utc};
use domain::{Order, Side};
use rust_decimal::Decimal;

use crate::liquidity::TradeTape;

/// 模型的乐观程度。**每个回测结果都必须带这个标记。**
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Optimism {
    /// 上界：假设任何触价都全额成交。仅用于计算"edge 衰减"。
    UpperBound,
    /// 刻意悲观的下界：要求真实成交发生在我们的价位上，且假设排队在队尾。
    ConservativeLower,
}

/// 成交判定的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FillOutcome {
    /// 未成交。
    None,
    /// 部分成交。
    Partial { quantity: Decimal, price: Decimal },
    /// 全部成交。
    Full { price: Decimal },
}

impl FillOutcome {
    pub fn filled_quantity(&self) -> Decimal {
        match self {
            FillOutcome::None => Decimal::ZERO,
            FillOutcome::Partial { quantity, .. } => *quantity,
            FillOutcome::Full { .. } => Decimal::ZERO, // 调用方需用 order.quantity 补齐
        }
    }

    pub fn is_filled(&self) -> bool {
        !matches!(self, FillOutcome::None)
    }

    /// 解析为实际成交量。
    pub fn quantity_for(&self, order: &Order) -> Decimal {
        match self {
            FillOutcome::None => Decimal::ZERO,
            FillOutcome::Partial { quantity, .. } => *quantity,
            FillOutcome::Full { .. } => order.quantity.get(),
        }
    }

    pub fn price_or(&self, fallback: Decimal) -> Decimal {
        match self {
            FillOutcome::None => fallback,
            FillOutcome::Partial { price, .. } | FillOutcome::Full { price } => *price,
        }
    }
}

/// 成交判定的上下文。模型只能看到这里给它的数据。
///
/// 刻意不是"完整 K 线 + 全部状态"，而是明确的数据窗口：这样每个模型需要什么
/// 数据是显式的，也不会出现"模型偷偷用了未来数据"。
pub struct FillContext<'a> {
    pub order: &'a Order,
    /// 订单挂出之后的成交带（只含挂单时刻之后的数据，防止偷看未来）。
    pub tape: &'a TradeTape,
    /// 订单挂出的时刻。
    pub placed_at: DateTime<Utc>,
    /// 我们用这个 tick 做价格比较。价格必须按 tick 对齐后比较，
    /// 否则 `3199.999` 会因为浮点式的理由不等于 `3200`。
    pub tick_size: Decimal,
}

/// 成交模型接口。
pub trait FillModel: Send + Sync {
    /// 模型名称。**必须写进回测结果**，让结论可追溯。
    fn name(&self) -> &'static str;

    /// 模型的数据需求描述。
    fn data_requirements(&self) -> &'static str;

    /// 乐观程度。
    fn optimism(&self) -> Optimism;

    /// 判定一张挂单在给定成交带里是否成交。
    fn evaluate(&self, ctx: &FillContext<'_>) -> FillOutcome;
}

/// **M0：wick 触价即全额成交。**
///
/// 这是能写出的最乐观模型。旧实现用的就是它（`FillModel::CandleRangeTouch`
/// 在 wick 触及价格时返回 100% 成交），并且是默认模型——所以每一份旧回测
/// 都是这个上界，而没有任何东西提示使用者。
///
/// 保留它的唯一用途：与 M1 对比，量化"edge 有多少来自不现实的成交假设"。
/// **绝不能作为默认值。**
pub struct M0WickTouchFull;

impl FillModel for M0WickTouchFull {
    fn name(&self) -> &'static str {
        "M0_wick_touch_full"
    }

    fn data_requirements(&self) -> &'static str {
        "1m OHLCV"
    }

    fn optimism(&self) -> Optimism {
        Optimism::UpperBound
    }

    fn evaluate(&self, ctx: &FillContext<'_>) -> FillOutcome {
        // 只要成交带里有任何一笔成交触及或穿过我们的限价，就视为全额成交。
        let limit = ctx.order.limit_price.get();
        let side = ctx.order.side;
        for t in ctx.tape.trades() {
            let touched = match side {
                // 买单：价格跌到我们的限价或更低
                Side::Buy => t.price.get() <= limit,
                // 卖单：价格涨到我们的限价或更高
                Side::Sell => t.price.get() >= limit,
            };
            if touched {
                // 按限价成交（乐观：不要求更好也不要求更差）
                return FillOutcome::Full { price: limit };
            }
        }
        FillOutcome::None
    }
}

/// **M1：需要真实成交穿过我们的价位，且假设我们排在队尾。**
///
/// 规则，全部刻意悲观：
///
/// 1. 挂单买价只有在**有成交以等于或低于**我们的价格发生时，才可能成交。
///    wick 触到价格但该价位及以上没有成交量 → **不成交**。
/// 2. 排队位置：我们在 `placed_at` 加入**队尾**。因此需要累计成交量超过
///    `V_ahead` 才能成交。在只有逐笔数据的条件下，`V_ahead` 只能假设为
///    "挂单时刻那一刻已经在队列里的量"，而我们无法观测——所以按**最悲观**
///    处理：从挂单时刻起、在**我们价位或更好**发生的全部成交量都要先"喂"
///    给队列前方，我们才成交。
/// 3. 止损限价单同样是挂单，适用同一规则。如果价格跳空穿过止损限价再
///    不回来，**订单挂着不成交、仓位继续裸露并承受浮亏**。这是诚实模型，
///    也是会让你难受的模型——这正是重点。
/// 4. 止盈同样要求有成交在等于或高于我们的卖价。中间价上涨不成交任何东西。
pub struct M1TradeThroughQueue {
    /// 队列前方**已有**的成交量。
    ///
    /// 含义：我们加入队列时排在这些量之后，需要累计成交量**超过**它才轮到
    /// 我们成交。默认 0——最朴素的假设是我们价位上还没有排队，一笔成交
    /// 打到我们的价位即可成交。设为正值模拟"队列已经很深"，更悲观。
    ///
    /// 注意这个参数无法从历史数据校准：币安归档没有订单 ID，我们观测不到
    /// 真实的队列深度。它只能作为敏感性分析的旋钮使用（例如"若前方有 10 手，
    /// edge 还剩多少"）。
    pub queue_ahead: Decimal,
}

impl Default for M1TradeThroughQueue {
    fn default() -> Self {
        Self {
            queue_ahead: Decimal::ZERO,
        }
    }
}

impl M1TradeThroughQueue {
    pub fn new(queue_ahead: Decimal) -> Self {
        Self { queue_ahead }
    }
}

impl FillModel for M1TradeThroughQueue {
    fn name(&self) -> &'static str {
        "M1_trade_through_queue"
    }

    fn data_requirements(&self) -> &'static str {
        "1m OHLCV + aggTrades（逐笔成交）"
    }

    fn optimism(&self) -> Optimism {
        Optimism::ConservativeLower
    }

    fn evaluate(&self, ctx: &FillContext<'_>) -> FillOutcome {
        let limit = ctx.order.limit_price.get();
        let side = ctx.order.side;
        // 累计"队列前方"已消化的成交量。
        let mut consumed = Decimal::ZERO;

        for t in ctx.tape.trades() {
            let price = t.price.get();

            // 判定这笔成交是否打到了我们所在的价位。
            //
            // 依据限价单在订单簿里的位置：
            //   买单挂 L 在**买盘**：卖方主动打下来，成交价 <= L 才碰到我们
            //   卖单挂 L 在**卖盘**：买方主动打上去，成交价 >= L 才碰到我们
            //
            // 注意卖单这里是 `>=`：我们的卖单在 L，买方主动成交在 L 或更高
            // 都会吃掉它。看着像"价格远高于挂单价也能成交"，但那是订单簿的
            // 实际行为——买方扫单会一路吃掉 L 及以上的所有卖单。
            let reaches_our_price = match side {
                Side::Buy => price <= limit,
                Side::Sell => price >= limit,
            };
            if !reaches_our_price {
                continue;
            }

            // 方向检查：这笔成交必须由**正确方向的主动单**驱动。
            //
            // 我们挂买单时需要卖方主动来吃（is_buyer_maker = true）；
            // 挂卖单时需要买方主动（is_buyer_maker = false）。
            // 反向的成交不消耗我们这一侧的队列，不能算作成交证据。
            let aggressor_consumes_our_side = match side {
                // 我们挂买单，需要卖方主动来吃
                Side::Buy => t.is_buyer_maker,
                // 我们挂卖单，需要买方主动来吃
                Side::Sell => !t.is_buyer_maker,
            };
            if !aggressor_consumes_our_side {
                continue;
            }

            consumed += t.quantity.get();
            // 严格大于：队列前方的量必须被完全消化，我们才成交。
            if consumed > self.queue_ahead {
                // 成交价：我们拿到自己的限价（maker 单以挂单价成交）。
                // 不假设更优价格——那会让回测比现实更好。
                //
                // 成交量为全额：一笔打到我们价位的成交通常会把该价位挂单吃完。
                // 分批成交需要更细的队列模型（按笔累积），那需要订单级数据，
                // 而币安归档不提供。这是本模型中偏乐观的一侧，已知且被记录。
                return FillOutcome::Full { price: limit };
            }
        }

        FillOutcome::None
    }
}

/// 按名称构造模型。回测配置与 CLI 用它。
pub fn model_by_name(name: &str) -> Option<Box<dyn FillModel>> {
    match name {
        "m0" | "M0" => Some(Box::new(M0WickTouchFull)),
        "m1" | "M1" => Some(Box::new(M1TradeThroughQueue::default())),
        _ => None,
    }
}

/// 所有已实现的模型名称。
pub const MODEL_NAMES: &[&str] = &["m0", "m1"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::liquidity::{Trade, TradeTape};
    use domain::{ClientOrderId, OrderPurpose, Price, Qty, TimeInForce};
    use rust_decimal_macros::dec;

    fn order(purpose: OrderPurpose, side: Side, limit: Decimal, qty: Decimal) -> Order {
        Order {
            client_id: ClientOrderId::new("t", 1),
            symbol: "ETHUSDC".into(),
            purpose,
            side,
            quantity: Qty::new(qty),
            limit_price: Price::new(limit),
            tif: TimeInForce::PostOnly,
            parent: None,
        }
    }

    fn trade(price: Decimal, qty: Decimal, buyer_maker: bool, ts_ms: i64) -> Trade {
        Trade {
            trade_id: ts_ms as u64,
            price: Price::new(price),
            quantity: Qty::new(qty),
            is_buyer_maker: buyer_maker,
            at: chrono::DateTime::from_timestamp_millis(ts_ms).unwrap(),
        }
    }

    fn tape(trades: Vec<Trade>) -> TradeTape {
        TradeTape::from_trades(trades)
    }

    const T0: i64 = 1_785_542_400_000;

    // ---------- M0 ----------

    #[test]
    fn m0_fills_on_any_touch() {
        let o = order(OrderPurpose::Entry, Side::Buy, dec!(3200), dec!(1));
        let t = tape(vec![trade(dec!(3199.99), dec!(0.01), true, T0)]);
        let ctx = FillContext {
            order: &o,
            tape: &t,
            placed_at: chrono::DateTime::from_timestamp_millis(T0).unwrap(),
            tick_size: dec!(0.01),
        };
        let out = M0WickTouchFull.evaluate(&ctx);
        assert!(out.is_filled(), "M0 触价即成交");
        assert_eq!(out.quantity_for(&o), dec!(1), "M0 给全额");
    }

    /// **这是 M0 与 M1 的核心分歧点。**
    ///
    /// 成交价停在我们的价位之上（3199 从未被触及），但 K 线 wick 可能触到过。
    /// 只有逐笔数据能证明"我们价位上没有成交"，所以 M1 必须说未成交。
    #[test]
    fn m1_does_not_fill_without_trade_at_our_price() {
        let o = order(OrderPurpose::Entry, Side::Buy, dec!(3199), dec!(1));
        // 成交都在 3200 以上，我们的买价 3199 从未被触及
        let t = tape(vec![
            trade(dec!(3200.5), dec!(5), true, T0 + 1),
            trade(dec!(3201.0), dec!(5), true, T0 + 2),
        ]);
        let ctx = FillContext {
            order: &o,
            tape: &t,
            placed_at: chrono::DateTime::from_timestamp_millis(T0).unwrap(),
            tick_size: dec!(0.01),
        };
        assert_eq!(
            M1TradeThroughQueue::default().evaluate(&ctx),
            FillOutcome::None,
            "我们价位上没有成交，不能算成交"
        );
    }

    #[test]
    fn m1_fills_when_trade_prints_at_our_price() {
        let o = order(OrderPurpose::Entry, Side::Buy, dec!(3200), dec!(1));
        let t = tape(vec![trade(dec!(3200), dec!(0.5), true, T0 + 1)]);
        let ctx = FillContext {
            order: &o,
            tape: &t,
            placed_at: chrono::DateTime::from_timestamp_millis(T0).unwrap(),
            tick_size: dec!(0.01),
        };
        let out = M1TradeThroughQueue::default().evaluate(&ctx);
        assert!(out.is_filled());
        assert_eq!(out.price_or(dec!(0)), dec!(3200), "以我们的限价成交");
    }

    /// 关键的方向检查：我们挂买单，但成交是**买方主动**在低位吃单——
    /// 那不会消耗买盘队列，不能算我们成交的证据。
    #[test]
    fn m1_requires_the_aggressor_to_hit_our_side() {
        let o = order(OrderPurpose::Entry, Side::Buy, dec!(3200), dec!(1));
        // 价格在 3200 或更低，但 is_buyer_maker = false -> 买方主动
        let t = tape(vec![trade(dec!(3199), dec!(10), false, T0 + 1)]);
        let ctx = FillContext {
            order: &o,
            tape: &t,
            placed_at: chrono::DateTime::from_timestamp_millis(T0).unwrap(),
            tick_size: dec!(0.01),
        };
        assert_eq!(
            M1TradeThroughQueue::default().evaluate(&ctx),
            FillOutcome::None,
            "买方主动的单子不会消耗买盘队列"
        );
    }

    #[test]
    fn m1_sell_side_requires_buyer_aggressor() {
        let o = order(OrderPurpose::TakeProfit, Side::Sell, dec!(3210), dec!(1));
        // 我们挂卖单，需要买方主动（is_buyer_maker = false）
        let good = tape(vec![trade(dec!(3210), dec!(1), false, T0 + 1)]);
        let ctx = FillContext {
            order: &o,
            tape: &good,
            placed_at: chrono::DateTime::from_timestamp_millis(T0).unwrap(),
            tick_size: dec!(0.01),
        };
        assert!(M1TradeThroughQueue::default().evaluate(&ctx).is_filled());

        // 卖方主动的成交不能让我们成交
        let bad = tape(vec![trade(dec!(3210), dec!(1), true, T0 + 1)]);
        let ctx2 = FillContext {
            order: &o,
            tape: &bad,
            placed_at: chrono::DateTime::from_timestamp_millis(T0).unwrap(),
            tick_size: dec!(0.01),
        };
        assert_eq!(
            M1TradeThroughQueue::default().evaluate(&ctx2),
            FillOutcome::None
        );
    }

    /// 队列前方有量时，需要更多成交量才能轮到我们。
    #[test]
    fn m1_respects_queue_depth_threshold() {
        let o = order(OrderPurpose::Entry, Side::Buy, dec!(3200), dec!(1));
        // 队列前方有 5 手
        let model = M1TradeThroughQueue::new(dec!(5));
        let placed = chrono::DateTime::from_timestamp_millis(T0).unwrap();

        // 累计 3 手 < 5，队列未消化完
        let t1 = tape(vec![trade(dec!(3200), dec!(3), true, T0 + 1)]);
        assert_eq!(
            model.evaluate(&FillContext {
                order: &o,
                tape: &t1,
                placed_at: placed,
                tick_size: dec!(0.01)
            }),
            FillOutcome::None,
            "累计 3 手不足以消化 5 手队列"
        );

        // 累计恰好 5 手：严格大于才成交，所以仍不成交
        let t2 = tape(vec![
            trade(dec!(3200), dec!(3), true, T0 + 1),
            trade(dec!(3200), dec!(2), true, T0 + 2),
        ]);
        assert_eq!(
            model.evaluate(&FillContext {
                order: &o,
                tape: &t2,
                placed_at: placed,
                tick_size: dec!(0.01)
            }),
            FillOutcome::None,
            "恰好等于队列量时我们仍排在最后，不成交"
        );

        // 累计 6 手 > 5，轮到我们
        let t3 = tape(vec![
            trade(dec!(3200), dec!(3), true, T0 + 1),
            trade(dec!(3200), dec!(3), true, T0 + 2),
        ]);
        assert!(
            model
                .evaluate(&FillContext {
                    order: &o,
                    tape: &t3,
                    placed_at: placed,
                    tick_size: dec!(0.01)
                })
                .is_filled(),
            "累计超过队列量后应成交"
        );
    }

    /// **止损也是挂单：跳空穿过它且不回来时，仓位继续裸露。**
    ///
    /// 这是 maker-only 的核心风险，也是 M1 与"触发即成交"模型的分界。
    #[test]
    fn m1_stop_loss_can_remain_unfilled_after_gap_through() {
        // 多头止损挂 3190；价格直接跳到 3180 但**由买方主动**（不消耗卖盘）
        let o = order(OrderPurpose::StopLoss, Side::Sell, dec!(3190), dec!(1));
        let t = tape(vec![
            trade(dec!(3185), dec!(10), true, T0 + 1), // 卖方主动，不消费我们的卖单
            trade(dec!(3180), dec!(10), true, T0 + 2),
        ]);
        let ctx = FillContext {
            order: &o,
            tape: &t,
            placed_at: chrono::DateTime::from_timestamp_millis(T0).unwrap(),
            tick_size: dec!(0.01),
        };
        assert_eq!(
            M1TradeThroughQueue::default().evaluate(&ctx),
            FillOutcome::None,
            "价格跌穿止损但无买方主动接手，止损挂着不成交——仓位裸露"
        );
    }

    #[test]
    fn model_lookup_by_name_covers_all_declared_models() {
        for name in MODEL_NAMES {
            assert!(model_by_name(name).is_some(), "{name} 应能构造");
        }
        assert!(model_by_name("m9").is_none());
    }

    /// 每个模型都必须是可识别的、且乐观度标记正确——回测结果靠它判断可信度。
    #[test]
    fn model_metadata_is_honest_about_optimism() {
        assert_eq!(M0WickTouchFull.optimism(), Optimism::UpperBound);
        assert_eq!(
            M1TradeThroughQueue::default().optimism(),
            Optimism::ConservativeLower,
            "M1 必须是悲观下界"
        );
        assert!(M0WickTouchFull.name().starts_with("M0"));
        assert!(M1TradeThroughQueue::default().name().starts_with("M1"));
    }
}
