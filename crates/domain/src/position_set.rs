//! 持仓与保护单的一致性管理。
//!
//! # 这是本软件存在的核心理由
//!
//! 币安 USDⓈ-M 合约**没有 OCO**，也没有 bracket 单。它支持的条件单只有：
//!
//! ```text
//! STOP / STOP_MARKET / TAKE_PROFIT / TAKE_PROFIT_MARKET / TRAILING_STOP_MARKET
//! ```
//!
//! 一张条件单只能对应一个数量、一个触发价。所以下面这些事币安原生做不到，
//! 必须由客户端实现：
//!
//! - **一个点位挂单，同时挂分批止盈与止损**（没有 bracket）
//! - **分批止盈**（40% / 30% / 30%），三张单分开挂、分别成交
//! - **保本止损**：浮盈后把止损推到入场价，需要撤单重挂
//! - **止损成交后撤销未成交的止盈单**（没有 OCO 自动做这件事）
//!
//! # 必须守住的不变量
//!
//! > **所有在挂的 reduce-only 单数量之和，不能超过当前持仓量。**
//!
//! 这条听起来显然，但分批止盈让它变得容易破坏：第一档止盈成交平掉 40% 后，
//! 止损单还挂着原始数量。等到止损触发时，它要平的数量超过了剩余持仓，币安
//! 会拒单（reduce-only 防止反向开仓）——**结果是止损失效，仓位裸露**。
//!
//! 本模块把"持仓 + 保护单"作为一个整体来管，每次成交后重新计算并给出调整
//! 动作。`check_consistency` 可以把这条不变量变成可断言的事实。

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::error::DomainError;
use crate::money::{Price, Qty};
use crate::order::{ClientOrderId, Effect, Order, OrderPurpose, Side};
use crate::protection::{ProtectionPlan, TpPlan};
use crate::state::{Position, TrackedOrder};

/// 一张保护单的角色。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtectionRole {
    /// 止损。
    Stop,
    /// 第 i 档止盈（从 0 开始）。
    TakeProfit { rung: usize },
}

/// 一个持仓连同它的保护单集合。
///
/// 这是"一笔交易"的完整状态。所有对保护单的调整都通过本类型的方法进行，
/// 从而保证 `check_consistency` 的不变量始终成立。
#[derive(Clone, Debug)]
pub struct PositionSet {
    pub symbol: String,
    pub side: Side,
    /// 当前持仓量（随止盈/止损成交而减少）。
    pub quantity: Qty,
    /// 入场时的原始持仓量。
    ///
    /// **各档止盈的数量基准必须是这个值，不是 `quantity`。**
    /// 若按递减后的 `quantity` 计算，第二档会基于缩小后的基数再乘比例，
    /// 导致总平仓量永远到不了 100%，留下无法平掉的残仓。
    /// 引擎里曾因把 `quantity` 当成基准而实际触发过这个 bug。
    pub original_quantity: Qty,
    pub entry_price: Price,
    pub opened_at: DateTime<Utc>,
    /// 保护单计划（决定止损与分批止盈的参数）。
    pub plan: ProtectionPlan,
    pub tp: TpPlan,
    /// 已完整成交的止盈档位。
    pub completed_rungs: Vec<usize>,
    /// 当前止损价（保本/移动止损会更新）。
    pub stop_price: Option<Price>,
    /// 止损是否已触发但未成交（maker-only 的裸露风险）。
    pub stop_triggered_at: Option<DateTime<Utc>>,
    /// 保护单挂出的时刻。
    ///
    /// 与 `opened_at` 区分：保护单是开仓成交后才挂出去的，成交判定只能看
    /// 这之后的成交。用开仓时刻作为起点会让止损"成交"在历史成交上，导致
    /// 它瞬间触发、仓位立即被平掉。
    pub protection_placed_at: Option<DateTime<Utc>>,
    /// 各档止盈对应的订单 ID。
    ///
    /// 需要它是因为分批止盈的每一档都是一张独立订单，成交回报只能靠 ID
    /// 对应回档位。空表示尚未挂出（模拟盘在成交后才编译保护单）。
    pub tp_order_ids: Vec<ClientOrderId>,
}

impl PositionSet {
    pub fn new(position: &Position, plan: ProtectionPlan, tp: TpPlan) -> Self {
        Self {
            symbol: position.symbol.clone(),
            side: position.side,
            quantity: position.quantity,
            original_quantity: position.quantity,
            entry_price: position.entry_price,
            opened_at: position.opened_at,
            plan,
            tp,
            completed_rungs: Vec::new(),
            stop_price: position.stop_price,
            stop_triggered_at: None,
            protection_placed_at: None,
            tp_order_ids: Vec::new(),
        }
    }

    /// 记录各档止盈的订单 ID。
    pub fn set_tp_order_ids(&mut self, ids: Vec<ClientOrderId>) {
        self.tp_order_ids = ids;
    }

    /// 下一档待成交的止盈档位序号。
    pub fn next_rung(&self) -> Option<usize> {
        let total = self.tp.rungs().len();
        (0..total).find(|i| !self.completed_rungs.contains(i))
    }

    /// 某档止盈对应的数量。
    ///
    /// 注意：数量基准是**入场时的原始持仓量**，而不是当前持仓量——否则
    /// 第一档成交后，第二档的比例会基于缩小后的基数计算，导致总平仓量
    /// 不足（永远平不完）。
    pub fn rung_quantity(&self, rung: usize) -> Qty {
        let rungs = self.tp.rungs();
        match rungs.get(rung) {
            Some((_, fraction)) => Qty::new(self.original_quantity.get() * fraction),
            None => Qty::ZERO,
        }
    }

    /// 全部止盈档位合计的平仓比例。
    pub fn total_tp_fraction(&self) -> Decimal {
        self.tp.rungs().iter().map(|(_, f)| *f).sum()
    }
}

/// 一致性检查的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsistencyReport {
    /// 所有在挂 reduce-only 单的数量之和。
    pub working_reduce_only_qty: Decimal,
    /// 当前持仓量。
    pub position_qty: Decimal,
    /// 是否一致。
    pub ok: bool,
    /// 不一致时的说明。
    pub problem: Option<String>,
}

/// 校验"在挂平仓单总量 ≤ 持仓量"这条不变量。
///
/// 返回 `ok == false` 时必须采取行动（减少挂单数量），否则止损会因
/// 数量超过持仓而被交易所拒绝，导致保护失效。
pub fn check_consistency(position: &Position, working: &[&TrackedOrder]) -> ConsistencyReport {
    let sum: Decimal = working
        .iter()
        .filter(|t| {
            t.order.reduce_only()
                && matches!(
                    t.state,
                    crate::order::OrderState::Live
                        | crate::order::OrderState::PartiallyFilled { .. }
                )
        })
        .map(|t| {
            // 已部分成交的单，剩余可成交数量是下单量减去已成交量
            t.order.quantity.get() - t.filled.get()
        })
        .filter(|q| *q > Decimal::ZERO)
        .sum();

    let pos = position.quantity.get();
    let ok = sum <= pos;
    ConsistencyReport {
        working_reduce_only_qty: sum,
        position_qty: pos,
        ok,
        problem: (!ok).then(|| {
            format!(
                "在挂平仓单合计 {sum} 超过持仓 {pos}——止损触发时会被交易所拒绝，\
                 保护将失效。必须减少挂单数量。"
            )
        }),
    }
}

/// 重算保护单后要执行的动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtectionFix {
    /// 撤掉一张单。
    Cancel(ClientOrderId),
    /// 重新挂一张（数量已修正）。
    Replace(Box<Order>),
    /// 无需调整。
    Nothing,
}

/// 某一档止盈成交后的调整。
///
/// 这是最需要小心的地方：止盈平掉一部分仓位后，**止损单的数量必须同步减少**，
/// 否则止损触发时数量超过剩余持仓会被拒单。
pub fn on_take_profit_filled(
    set: &mut PositionSet,
    rung: usize,
    filled_qty: Qty,
    working: &[&TrackedOrder],
) -> Result<Vec<ProtectionFix>, DomainError> {
    if set.completed_rungs.contains(&rung) {
        // 重复事件：同一档只处理一次，否则会重复减仓
        return Ok(vec![ProtectionFix::Nothing]);
    }
    if rung >= set.tp.rungs().len() {
        return Err(DomainError::IllegalTransition(format!(
            "止盈档位 {rung} 超出计划范围（共 {} 档）",
            set.tp.rungs().len()
        )));
    }

    set.completed_rungs.push(rung);

    // 扣减持仓
    let new_qty = set.quantity.get() - filled_qty.get();
    if new_qty < Decimal::ZERO {
        return Err(DomainError::IllegalTransition(format!(
            "止盈成交 {filled_qty} 超过持仓 {}",
            set.quantity
        )));
    }
    set.quantity = Qty::new(new_qty);

    let mut fixes = Vec::new();
    if new_qty.is_zero() {
        // 仓位已平完：撤掉所有残留挂单（币安没有 OCO 会自动做这件事）
        for t in working {
            if t.state.is_open() {
                fixes.push(ProtectionFix::Cancel(t.order.client_id.clone()));
            }
        }
        return Ok(fixes);
    }

    // 按新的持仓量修正止损单数量
    if let Some(stop) = working
        .iter()
        .find(|t| t.order.purpose == OrderPurpose::StopLoss && t.state.is_open())
    {
        let want = new_qty;
        let have = stop.order.quantity.get() - stop.filled.get();
        if have != want {
            let mut corrected = stop.order.clone();
            corrected.quantity = Qty::new(want);
            fixes.push(ProtectionFix::Replace(Box::new(corrected)));
        }
    }

    // 判定是否触发保本止损：第一档止盈成交后即可考虑
    if let Some(be) = set.plan.break_even {
        if let Some(stop_price) = set.stop_price {
            let risk = (set.entry_price.get() - stop_price.get()).abs();
            // 分批止盈的第一档成交本身就意味着价格已经走出了一段
            let reached = set.tp.rungs()[rung].0 * set.entry_price.get();
            if risk > Decimal::ZERO && reached >= risk * be.trigger_r {
                let new_stop = match set.side {
                    Side::Buy => set.entry_price.get() + be.offset,
                    Side::Sell => set.entry_price.get() - be.offset,
                };
                let better = match set.side {
                    Side::Buy => new_stop > stop_price.get(),
                    Side::Sell => new_stop < stop_price.get(),
                };
                if better {
                    set.stop_price = Some(Price::new(new_stop));
                }
            }
        }
    }

    Ok(fixes)
}

/// 止损成交后的调整：撤掉全部未成交的止盈单。
///
/// 币安没有 OCO，所以"一个触发另一个自动取消"必须我们自己做。漏掉这一步
/// 会让止损平仓后止盈单仍在挂——它们带 reduceOnly，因此会被交易所拒绝，
/// 但会持续占用挂单额度并污染状态。
pub fn on_stop_filled(set: &mut PositionSet, working: &[&TrackedOrder]) -> Vec<ProtectionFix> {
    let mut fixes = Vec::new();
    for t in working {
        if t.state.is_open() && t.order.purpose == OrderPurpose::TakeProfit {
            fixes.push(ProtectionFix::Cancel(t.order.client_id.clone()));
        }
    }
    set.quantity = Qty::ZERO;
    fixes
}

/// 仓位归零后的清理：撤掉所有残留挂单。
pub fn on_position_closed(working: &[&TrackedOrder]) -> Vec<ProtectionFix> {
    working
        .iter()
        .filter(|t| t.state.is_open())
        .map(|t| ProtectionFix::Cancel(t.order.client_id.clone()))
        .collect()
}

/// 把调整动作转成状态机副作用。
pub fn fixes_to_effects(fixes: Vec<ProtectionFix>) -> Vec<Effect> {
    fixes
        .into_iter()
        .filter_map(|f| match f {
            ProtectionFix::Cancel(id) => Some(Effect::Cancel(id)),
            ProtectionFix::Replace(o) => Some(Effect::Submit(o)),
            ProtectionFix::Nothing => None,
        })
        .collect()
}

/// 持仓与保护单的完整快照，供界面展示。
///
/// 手动面板必须能看到这些：剩余仓位、各档止盈是否已成交、止损当前挂在哪。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PositionView {
    pub symbol: String,
    pub side: Side,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub entry_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub unrealized_pnl: Decimal,
    pub stop_price: Option<Price>,
    pub stop_triggered: bool,
    /// 各档止盈的状态。
    pub rungs: Vec<RungView>,
    /// 已实现盈亏（含手续费）。
    #[serde(with = "rust_decimal::serde::str")]
    pub realized_pnl: Decimal,
}

/// 一档止盈的展示状态。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RungView {
    pub rung: usize,
    /// 距入场价的百分比。
    #[serde(with = "rust_decimal::serde::str")]
    pub pct: Decimal,
    /// 该档平仓比例。
    #[serde(with = "rust_decimal::serde::str")]
    pub fraction: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    pub filled: bool,
}

impl PositionSet {
    /// 构造界面展示用的快照。
    pub fn view(&self, mark_price: Price, realized_pnl: Decimal) -> PositionView {
        let entry = self.entry_price.get();
        let mark = mark_price.get();
        let unrealized = match self.side {
            Side::Buy => (mark - entry) * self.quantity.get(),
            Side::Sell => (entry - mark) * self.quantity.get(),
        };

        let rungs = self
            .tp
            .rungs()
            .iter()
            .enumerate()
            .map(|(i, (pct, fraction))| {
                let price = match self.side {
                    Side::Buy => entry * (Decimal::ONE + pct),
                    Side::Sell => entry * (Decimal::ONE - pct),
                };
                RungView {
                    rung: i,
                    pct: *pct,
                    fraction: *fraction,
                    price,
                    filled: self.completed_rungs.contains(&i),
                }
            })
            .collect();

        PositionView {
            symbol: self.symbol.clone(),
            side: self.side,
            quantity: self.quantity.get(),
            entry_price: entry,
            unrealized_pnl: unrealized,
            stop_price: self.stop_price,
            stop_triggered: self.stop_triggered_at.is_some(),
            rungs,
            realized_pnl,
        }
    }
}

/// 手动下单计划。界面构造它，后端编译成具体订单。
///
/// **前端绝不计算价格**——它只声明意图，量化、保护单推导、风控都由后端完成。
/// 这样"第四份止盈公式"在结构上不可能出现。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManualPlan {
    pub symbol: String,
    pub side: Side,
    /// 期望的入场价（未量化）。
    #[serde(with = "rust_decimal::serde::str")]
    pub entry: Decimal,
    /// 期望数量（未量化）。`None` 表示按 `size_pct` 与杠杆计算。
    pub quantity: Option<Qty>,
    /// 按权益比例下单时的比例。
    pub size_pct: Option<Decimal>,
    pub leverage: Decimal,
    /// 止损（未量化）。
    pub stop: Decimal,
    /// 分批止盈。
    pub take_profit: TpPlan,
    /// 保本止损。
    pub break_even: Option<crate::protection::BreakEvenSpec>,
    /// 移动止损。
    pub trailing: Option<crate::protection::TrailingSpec>,
    /// 挂单超时自动撤销的时刻。
    pub cancel_unfilled_after: Option<DateTime<Utc>>,
    /// 客户端引用，用于订单 ID 的前缀，便于在交易所侧辨认。
    pub client_ref: String,
}

/// 手动计划的预览结果：量化后的具体价位与风控裁决。
///
/// 界面渲染的就是这些值——用户看到的价与将要挂出的价完全一致，因为
/// `/preview` 与 `/submit` 走的是同一条编译路径。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManualPreview {
    /// 量化后的入场价。
    pub entry: Price,
    /// 量化后各档止盈价与数量。
    pub take_profits: Vec<RungPreview>,
    /// 量化后的止损价。
    pub stop: Price,
    /// 量化后的数量。
    pub quantity: Qty,
    /// 名义价值。
    #[serde(with = "rust_decimal::serde::str")]
    pub notional: Decimal,
    /// 预估保证金占用。
    #[serde(with = "rust_decimal::serde::str")]
    pub margin_required: Decimal,
    /// 止损距估算强平价的缓冲比例。`None` 表示无法估算（杠杆过高）。
    pub liquidation_buffer_pct: Option<Decimal>,
    /// 风控是否通过。
    pub accepted: bool,
    /// 拒绝原因（面向用户的中文说明）。
    pub reject_reason: Option<String>,
    /// 警告（不阻断，但要显示）。
    pub warnings: Vec<String>,
}

/// 一档止盈的预览。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RungPreview {
    pub rung: usize,
    pub price: Price,
    pub quantity: Qty,
    /// 该档的毛利润（不含手续费）。
    #[serde(with = "rust_decimal::serde::str")]
    pub gross_profit: Decimal,
    /// 距入场价的基点。
    #[serde(with = "rust_decimal::serde::str")]
    pub distance_bp: Decimal,
}

/// 构建手动计划的预览。
///
/// 这是**唯一**把 `ManualPlan` 变成具体价格的函数，`/preview` 与 `/submit`
/// 都调用它，所以界面看到的与实际的必然一致。
pub fn preview_manual(
    instrument: &crate::instrument::Instrument,
    plan: &ManualPlan,
    equity: Decimal,
    mark_price: Decimal,
    limits: &crate::risk::RiskLimits,
) -> Result<ManualPreview, DomainError> {
    use crate::precision::PriceRole;

    let mut warnings = Vec::new();

    // 入场价：被动挂单，朝远离对手价的方向取整
    let entry = instrument
        .precision
        .price_for(plan.side, plan.entry, PriceRole::PassiveEntry)?;
    let close_side = plan.side.opposite();

    // 数量。
    //
    // 用 `quantity_or_zero` 而非 `quantity`：用户把数量填得太小时，应该得到
    // 「数量低于最小步长，无法下单」这条可读的拒绝原因，而不是一个技术性错误。
    let quantity = match plan.quantity {
        Some(q) => instrument.precision.quantity_or_zero(q.get())?,
        None => {
            let pct = plan.size_pct.unwrap_or(Decimal::new(1, 1));
            let notional = equity * pct * plan.leverage;
            if entry.get() <= Decimal::ZERO {
                return Err(DomainError::NonPositivePrice(entry.get()));
            }
            instrument
                .precision
                .quantity_or_zero(notional / entry.get())?
        }
    };

    if quantity.is_zero() {
        return Ok(ManualPreview {
            entry,
            take_profits: vec![],
            stop: entry,
            quantity,
            notional: Decimal::ZERO,
            margin_required: Decimal::ZERO,
            liquidation_buffer_pct: None,
            accepted: false,
            reject_reason: Some("数量低于交易所最小步长，无法下单".into()),
            warnings,
        });
    }

    let notional = quantity.get() * entry.get();
    let margin_required = notional / plan.leverage.max(Decimal::ONE);

    // 止损
    let stop = instrument
        .precision
        .price_for(close_side, plan.stop, PriceRole::StopLoss)?;

    // 分批止盈
    plan.take_profit.validate()?;
    let mut take_profits = Vec::new();
    for (i, (pct, fraction)) in plan.take_profit.rungs().iter().enumerate() {
        let raw = match plan.side {
            Side::Buy => entry.get() * (Decimal::ONE + pct),
            Side::Sell => entry.get() * (Decimal::ONE - pct),
        };
        let price = instrument
            .precision
            .price_for(close_side, raw, PriceRole::TakeProfit)?;
        let qty = instrument
            .precision
            .quantity_or_zero(quantity.get() * fraction)?;
        if qty.is_zero() {
            warnings.push(format!(
                "第 {} 档止盈数量低于交易所最小步长，会被跳过",
                i + 1
            ));
            continue;
        }
        let gross = match plan.side {
            Side::Buy => (price.get() - entry.get()) * qty.get(),
            Side::Sell => (entry.get() - price.get()) * qty.get(),
        };
        take_profits.push(RungPreview {
            rung: i,
            price,
            quantity: qty,
            gross_profit: gross,
            distance_bp: pct * Decimal::from(10_000),
        });
    }

    // 风控
    let tp_first = take_profits
        .first()
        .map(|r| r.price.get())
        .unwrap_or(entry.get());
    let verdict = crate::risk::check_entry(
        instrument,
        plan.side,
        entry.get(),
        stop.get(),
        tp_first,
        plan.leverage,
        limits,
    );
    let (accepted, reject_reason) = match verdict {
        crate::risk::RiskVerdict::Pass => (true, None),
        crate::risk::RiskVerdict::Reject(r) => (false, Some(r.message().to_string())),
    };

    // 强平缓冲提示
    let is_long = plan.side == Side::Buy;
    let liq = instrument.liquidation_price_estimate(entry.get(), plan.leverage, is_long);
    let liquidation_buffer_pct = liq.map(|l| (l - stop.get()).abs() / entry.get());
    if let Some(b) = liquidation_buffer_pct {
        if b < Decimal::new(1, 3) {
            warnings.push(format!(
                "止损距估算强平价仅 {}，仓位在此止损前有被强平的风险",
                b
            ));
        }
    }

    // triggerProtect 提示。
    //
    // 币安的条件单（STOP / TAKE_PROFIT 系列）要求触发价距标记价至少 5%，
    // 否则直接拒单。但**本系统的出场单全部是 GTX 限价单，不是条件单**，
    // 所以这条约束通常不适用。
    //
    // 唯一需要提示的情形是：当价格已经走到止盈或止损价位附近时，用户若
    // 想改用条件单（例如为了确保触发后必定成交），会撞上这条限制。所以
    // 这里只在**有明显偏离**时给一条说明性提示，而不是每次都报警。
    //
    // 早先的实现无条件报告"距标记价不足 5%"——在限价单场景下这是误报，
    // 而且没有实时行情时标记价是默认值，判断本身也不可靠。
    if mark_price > Decimal::ZERO {
        const TRIGGER_PROTECT: Decimal = Decimal::from_parts(5, 0, 0, false, 2); // 0.05
        let tp_distance = ((tp_first - mark_price) / mark_price).abs();
        let stop_distance = ((stop.get() - mark_price) / mark_price).abs();
        if tp_distance < TRIGGER_PROTECT || stop_distance < TRIGGER_PROTECT {
            warnings.push(
                "止盈或止损价距当前市价较近。本系统的出场单用 GTX 限价单挂出，\
                 不受币安条件单触发保护（triggerProtect，距标记价需 ≥5%）的约束；\
                 但若你打算改用条件单，这两个价位会被拒单。"
                    .into(),
            );
        }
    }

    Ok(ManualPreview {
        entry,
        take_profits,
        stop,
        quantity,
        notional,
        margin_required,
        liquidation_buffer_pct,
        accepted,
        reject_reason,
        warnings,
    })
}

/// 按成交价统计各档止盈的分布，供界面画图。
pub fn rung_distribution(rungs: &[(ClientOrderId, ProtectionRole)]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for (id, role) in rungs {
        let key = match role {
            ProtectionRole::Stop => "止损".to_string(),
            ProtectionRole::TakeProfit { rung } => format!("止盈第 {} 档", rung + 1),
        };
        *m.entry(key).or_insert(0) += 1;
        let _ = id;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::{ContractKind, FeeSchedule, FeeSource, Instrument};
    use crate::precision::Precision;
    use crate::protection::{BreakEvenSpec, StopSpec, TpRung};
    use crate::state::TrackedOrder;
    use rust_decimal_macros::dec;

    fn instrument() -> Instrument {
        Instrument {
            symbol: "ETHUSDC".into(),
            kind: ContractKind::CryptoPerp,
            base_asset: "ETH".into(),
            quote_asset: "USDC".into(),
            margin_asset: "USDC".into(),
            settlement_asset: "USDC".into(),
            precision: Precision {
                tick_size: dec!(0.01),
                step_size: dec!(0.001),
                min_qty: dec!(0.001),
                min_notional: dec!(5),
            },
            maint_margin_pct: dec!(2.5),
            required_margin_pct: dec!(5),
            liquidation_fee: dec!(0.0125),
            fees: FeeSchedule {
                maker_rate: Decimal::ZERO,
                taker_rate: dec!(0.0005),
                source: FeeSource::PromotionalAssumed,
                observed_at: Utc::now(),
            },
        }
    }

    fn ladder() -> TpPlan {
        TpPlan::Ladder {
            rungs: vec![
                TpRung {
                    pct: dec!(0.0004),
                    fraction: dec!(0.4),
                },
                TpRung {
                    pct: dec!(0.0008),
                    fraction: dec!(0.3),
                },
                TpRung {
                    pct: dec!(0.0012),
                    fraction: dec!(0.3),
                },
            ],
        }
    }

    fn set() -> PositionSet {
        let pos = Position {
            symbol: "ETHUSDC".into(),
            side: Side::Buy,
            quantity: Qty::new(dec!(1)),
            entry_price: Price::new(dec!(3200)),
            opened_at: Utc::now(),
            stop_price: Some(Price::new(dec!(3190))),
        };
        PositionSet::new(
            &pos,
            ProtectionPlan {
                stop: StopSpec::Structural { price: dec!(3190) },
                break_even: Some(BreakEvenSpec {
                    trigger_r: Decimal::ONE,
                    offset: Decimal::ZERO,
                }),
                trailing: None,
                timed_cancel: None,
            },
            ladder(),
        )
    }

    fn tracked(
        id: &str,
        purpose: OrderPurpose,
        side: Side,
        qty: Decimal,
        filled: Decimal,
    ) -> TrackedOrder {
        TrackedOrder {
            order: Order {
                client_id: ClientOrderId(id.into()),
                symbol: "ETHUSDC".into(),
                purpose,
                side,
                quantity: Qty::new(qty),
                limit_price: Price::new(dec!(3200)),
                tif: crate::order::TimeInForce::PostOnly,
                parent: None,
            },
            state: crate::order::OrderState::Live,
            exchange_id: Some("E".into()),
            filled: Qty::new(filled),
            avg_price: None,
            updated_at: Utc::now(),
        }
    }

    /// **这是本模块存在的核心理由。**
    ///
    /// 分批止盈第一档平掉 40% 后，止损单还挂着原始 100% 的数量。如果不同步
    /// 修正，止损触发时会因数量超过剩余持仓而被交易所拒绝——**保护失效**。
    #[test]
    fn stop_quantity_is_reduced_after_partial_take_profit() {
        let mut s = set();
        let stop = tracked("stop", OrderPurpose::StopLoss, Side::Sell, dec!(1), dec!(0));
        let working = vec![&stop];

        // 第一档止盈成交 40%
        let fixes = on_take_profit_filled(&mut s, 0, Qty::new(dec!(0.4)), &working).unwrap();

        assert_eq!(s.quantity.get(), dec!(0.6), "持仓应减少到 60%");
        let replaced = fixes
            .iter()
            .find_map(|f| match f {
                ProtectionFix::Replace(o) if o.purpose == OrderPurpose::StopLoss => {
                    Some(o.quantity.get())
                }
                _ => None,
            })
            .expect("止损单数量必须被修正");
        assert_eq!(
            replaced,
            dec!(0.6),
            "止损数量必须同步到剩余持仓，否则触发时会被拒单"
        );
    }

    /// 各档止盈的数量基准是**入场时的原始持仓**，不是逐次递减后的持仓。
    ///
    /// 如果按递减基数算，第二档会基于 60% 再乘 30% = 18%，总平仓量永远
    /// 到不了 100%，会留下无法平掉的残仓。
    #[test]
    fn rung_quantities_use_original_quantity_as_base() {
        let s = set();
        assert_eq!(s.original_quantity.get(), dec!(1), "原始持仓量必须被记录");
        assert_eq!(s.rung_quantity(0).get(), dec!(0.4));
        assert_eq!(s.rung_quantity(1).get(), dec!(0.3));
        assert_eq!(s.rung_quantity(2).get(), dec!(0.3));

        let total: Decimal = (0..3).map(|i| s.rung_quantity(i).get()).sum();
        assert_eq!(total, dec!(1), "三档合计必须等于原始持仓，否则平不完");
    }

    /// 全部止盈成交后仓位归零，必须撤掉所有残留挂单——币安没有 OCO。
    #[test]
    fn closing_position_cancels_all_remaining_orders() {
        let mut s = set();
        let stop = tracked("stop", OrderPurpose::StopLoss, Side::Sell, dec!(1), dec!(0));
        let tp3 = tracked(
            "tp2",
            OrderPurpose::TakeProfit,
            Side::Sell,
            dec!(0.3),
            dec!(0),
        );
        let working = vec![&stop, &tp3];

        // 模拟三档全部成交
        on_take_profit_filled(&mut s, 0, Qty::new(dec!(0.4)), &working).unwrap();
        on_take_profit_filled(&mut s, 1, Qty::new(dec!(0.3)), &working).unwrap();
        let fixes = on_take_profit_filled(&mut s, 2, Qty::new(dec!(0.3)), &working).unwrap();

        assert_eq!(s.quantity.get(), Decimal::ZERO);
        let cancels = fixes
            .iter()
            .filter(|f| matches!(f, ProtectionFix::Cancel(_)))
            .count();
        assert_eq!(cancels, 2, "仓位平完后必须撤销全部残留挂单");
    }

    /// 止损成交后必须撤掉未成交的止盈单——没有 OCO 就得自己做。
    #[test]
    fn stop_fill_cancels_working_take_profits() {
        let mut s = set();
        let tp1 = tracked(
            "tp0",
            OrderPurpose::TakeProfit,
            Side::Sell,
            dec!(0.4),
            dec!(0),
        );
        let tp2 = tracked(
            "tp1",
            OrderPurpose::TakeProfit,
            Side::Sell,
            dec!(0.3),
            dec!(0),
        );
        let stop = tracked("stop", OrderPurpose::StopLoss, Side::Sell, dec!(1), dec!(1));
        let working = vec![&tp1, &tp2, &stop];

        let fixes = on_stop_filled(&mut s, &working);
        let cancels: Vec<_> = fixes
            .iter()
            .filter_map(|f| match f {
                ProtectionFix::Cancel(id) => Some(id.as_str().to_string()),
                _ => None,
            })
            .collect();

        assert_eq!(cancels.len(), 2, "两张未成交止盈单都要撤");
        assert!(cancels.contains(&"tp0".to_string()));
        assert!(cancels.contains(&"tp1".to_string()));
        assert!(
            !cancels.contains(&"stop".to_string()),
            "已成交的止损不该被撤"
        );
        assert_eq!(s.quantity.get(), Decimal::ZERO);
    }

    /// 重复的成交事件必须只处理一次，否则会重复扣减持仓。
    #[test]
    fn duplicate_take_profit_event_is_ignored() {
        let mut s = set();
        let stop = tracked("stop", OrderPurpose::StopLoss, Side::Sell, dec!(1), dec!(0));
        let working = vec![&stop];

        on_take_profit_filled(&mut s, 0, Qty::new(dec!(0.4)), &working).unwrap();
        let qty_after_first = s.quantity.get();

        // 同一档再来一次
        let fixes = on_take_profit_filled(&mut s, 0, Qty::new(dec!(0.4)), &working).unwrap();
        assert_eq!(s.quantity.get(), qty_after_first, "重复事件不应再次扣减");
        assert_eq!(fixes, vec![ProtectionFix::Nothing]);
    }

    /// 成交超过持仓是状态不一致，必须报错而不是悄悄接受。
    #[test]
    fn take_profit_exceeding_position_is_rejected() {
        let mut s = set();
        let err = on_take_profit_filled(&mut s, 0, Qty::new(dec!(5)), &[]).unwrap_err();
        assert!(matches!(err, DomainError::IllegalTransition(_)));
    }

    #[test]
    fn out_of_range_rung_is_rejected() {
        let mut s = set();
        assert!(on_take_profit_filled(&mut s, 9, Qty::new(dec!(0.1)), &[]).is_err());
    }

    /// 一致性检查必须能发现"挂单总量超过持仓"这个危险状态。
    #[test]
    fn consistency_check_detects_over_committed_orders() {
        let pos = Position {
            symbol: "ETHUSDC".into(),
            side: Side::Buy,
            quantity: Qty::new(dec!(0.6)),
            entry_price: Price::new(dec!(3200)),
            opened_at: Utc::now(),
            stop_price: None,
        };
        // 止损仍挂着 1.0，超过持仓 0.6
        let stop = tracked("stop", OrderPurpose::StopLoss, Side::Sell, dec!(1), dec!(0));
        let report = check_consistency(&pos, &[&stop]);
        assert!(!report.ok, "应检出超量挂单");
        assert!(report.problem.is_some());
        assert!(report.problem.unwrap().contains("失效"), "说明必须点出后果");
    }

    #[test]
    fn consistency_check_passes_when_matched() {
        let pos = Position {
            symbol: "ETHUSDC".into(),
            side: Side::Buy,
            quantity: Qty::new(dec!(0.6)),
            entry_price: Price::new(dec!(3200)),
            opened_at: Utc::now(),
            stop_price: None,
        };
        let stop = tracked(
            "stop",
            OrderPurpose::StopLoss,
            Side::Sell,
            dec!(0.6),
            dec!(0),
        );
        let report = check_consistency(&pos, &[&stop]);
        assert!(report.ok);
    }

    /// 部分成交的单，剩余可成交数量要正确计算。
    #[test]
    fn consistency_accounts_for_partially_filled_orders() {
        let pos = Position {
            symbol: "ETHUSDC".into(),
            side: Side::Buy,
            quantity: Qty::new(dec!(0.6)),
            entry_price: Price::new(dec!(3200)),
            opened_at: Utc::now(),
            stop_price: None,
        };
        // 挂 1.0，已成交 0.4 -> 剩余 0.6，与持仓相等
        let stop = tracked(
            "stop",
            OrderPurpose::StopLoss,
            Side::Sell,
            dec!(1),
            dec!(0.4),
        );
        let report = check_consistency(&pos, &[&stop]);
        assert!(report.ok, "剩余量 0.6 等于持仓，应通过：{report:?}");
        assert_eq!(report.working_reduce_only_qty, dec!(0.6));
    }

    /// 保本止损在浮盈足够后把止损推到入场价。
    #[test]
    fn break_even_moves_stop_after_first_rung() {
        let mut s = set();
        let stop = tracked("stop", OrderPurpose::StopLoss, Side::Sell, dec!(1), dec!(0));
        // 第一档 4bp = 3200*0.0004 = 1.28；止损距离 10 点，trigger_r=1 需要 10 点
        // 1.28 < 10，所以不应触发保本
        on_take_profit_filled(&mut s, 0, Qty::new(dec!(0.4)), &[&stop]).unwrap();
        assert_eq!(
            s.stop_price.unwrap().get(),
            dec!(3190),
            "浮盈不足 1R 时不应推保本止损"
        );
    }

    // ---------- 手动计划预览 ----------

    fn manual(side: Side, entry: Decimal, stop: Decimal) -> ManualPlan {
        ManualPlan {
            symbol: "ETHUSDC".into(),
            side,
            entry,
            quantity: Some(Qty::new(dec!(0.1))),
            size_pct: None,
            leverage: Decimal::from(3),
            stop,
            take_profit: ladder(),
            break_even: None,
            trailing: None,
            cancel_unfilled_after: None,
            client_ref: "manual".into(),
        }
    }

    /// 预览返回的价位必须就是将要挂出的价位——`/preview` 与 `/submit`
    /// 走同一条编译路径。
    #[test]
    fn preview_produces_quantized_prices() {
        let i = instrument();
        let p = manual(Side::Buy, dec!(3200), dec!(3190));
        let pv = preview_manual(
            &i,
            &p,
            dec!(10000),
            dec!(3200),
            &crate::risk::RiskLimits::default(),
        )
        .unwrap();

        assert_eq!(pv.entry.get(), dec!(3200));
        assert_eq!(pv.quantity.get(), dec!(0.1));
        assert_eq!(pv.take_profits.len(), 3);
        // 三档价格递增（平多止盈向上取整）
        assert!(pv.take_profits[0].price.get() < pv.take_profits[1].price.get());
        assert!(pv.take_profits[1].price.get() < pv.take_profits[2].price.get());
        // 数量合计等于下总量
        let total: Decimal = pv.take_profits.iter().map(|r| r.quantity.get()).sum();
        assert_eq!(total, dec!(0.1));
    }

    /// 数量低于最小步长时必须明确拒绝，而不是下出零数量订单。
    #[test]
    fn preview_rejects_zero_quantity() {
        let i = instrument();
        let mut p = manual(Side::Buy, dec!(3200), dec!(3190));
        p.quantity = Some(Qty::new(dec!(0.00001)));
        let pv = preview_manual(
            &i,
            &p,
            dec!(10000),
            dec!(3200),
            &crate::risk::RiskLimits::default(),
        )
        .unwrap();
        assert!(!pv.accepted);
        assert!(pv.reject_reason.is_some());
    }

    /// 止损方向错误必须被风控拦下。
    #[test]
    fn preview_rejects_stop_on_wrong_side() {
        let i = instrument();
        // 多头但止损放在入场价上方
        let p = manual(Side::Buy, dec!(3200), dec!(3210));
        let pv = preview_manual(
            &i,
            &p,
            dec!(10000),
            dec!(3200),
            &crate::risk::RiskLimits::default(),
        )
        .unwrap();
        assert!(!pv.accepted, "止损方向错误应被拒绝");
        assert!(pv.reject_reason.is_some());
    }

    /// 止损距标记价不足 5% 时必须警告——币安的 triggerProtect 会拒单。
    #[test]
    fn preview_warns_about_trigger_protect() {
        let i = instrument();
        let p = manual(Side::Buy, dec!(3200), dec!(3190));
        let pv = preview_manual(
            &i,
            &p,
            dec!(10000),
            dec!(3200),
            &crate::risk::RiskLimits::default(),
        )
        .unwrap();
        assert!(
            pv.warnings.iter().any(|w| w.contains("triggerProtect")),
            "应警告条件单触发保护：{:?}",
            pv.warnings
        );
    }

    /// 保证金占用与名义价值必须算对。
    #[test]
    fn preview_computes_margin_requirement() {
        let i = instrument();
        let p = manual(Side::Buy, dec!(3200), dec!(3190));
        let pv = preview_manual(
            &i,
            &p,
            dec!(10000),
            dec!(3200),
            &crate::risk::RiskLimits::default(),
        )
        .unwrap();
        assert_eq!(pv.notional, dec!(320), "0.1 * 3200");
        // 320 / 3 是循环小数，按显示精度比较
        assert_eq!(
            pv.margin_required.round_dp(2),
            dec!(106.67),
            "保证金 = 名义价值 / 杠杆"
        );
    }

    /// 空仓时强平缓冲应可估算，供界面提示。
    #[test]
    fn preview_reports_liquidation_buffer() {
        let i = instrument();
        let p = manual(Side::Buy, dec!(3200), dec!(3190));
        let pv = preview_manual(
            &i,
            &p,
            dec!(10000),
            dec!(3200),
            &crate::risk::RiskLimits::default(),
        )
        .unwrap();
        assert!(pv.liquidation_buffer_pct.is_some());
    }

    /// 按权益比例下单时数量应由权益与杠杆算出。
    #[test]
    fn preview_sizes_by_equity_fraction() {
        let i = instrument();
        let mut p = manual(Side::Buy, dec!(3200), dec!(3190));
        p.quantity = None;
        p.size_pct = Some(dec!(0.1));
        p.leverage = Decimal::from(3);
        let pv = preview_manual(
            &i,
            &p,
            dec!(10000),
            dec!(3200),
            &crate::risk::RiskLimits::default(),
        )
        .unwrap();
        // 10000 * 0.1 * 3 / 3200 = 0.9375 -> 向下量化 0.937
        assert_eq!(pv.quantity.get(), dec!(0.937));
    }

    #[test]
    fn rung_distribution_groups_by_role() {
        let rows = vec![
            (ClientOrderId::new("a", 1), ProtectionRole::Stop),
            (
                ClientOrderId::new("b", 1),
                ProtectionRole::TakeProfit { rung: 0 },
            ),
            (
                ClientOrderId::new("c", 1),
                ProtectionRole::TakeProfit { rung: 0 },
            ),
            (
                ClientOrderId::new("d", 1),
                ProtectionRole::TakeProfit { rung: 1 },
            ),
        ];
        let m = rung_distribution(&rows);
        assert_eq!(m.get("止损"), Some(&1));
        assert_eq!(m.get("止盈第 1 档"), Some(&2));
        assert_eq!(m.get("止盈第 2 档"), Some(&1));
    }
}
