//! 订单与成交的持久化。
//!
//! # 恢复语义
//!
//! 进程重启后必须能回答两个问题：
//! 1. **有哪些订单可能还在交易所挂着？** —— 决定要不要查询对账。
//! 2. **本地持仓与交易所是否一致？** —— 不一致时必须进入只减仓。
//!
//! 所以持久化的不是"我们以为的状态"，而是**订单的最后已知状态**。
//! `Unknown` 状态的订单必须能被查出来并触发对账，绝不能因为重启就丢掉。

use chrono::{DateTime, TimeZone, Utc};
use domain::{
    ClientOrderId, Fill, Order, OrderPurpose, OrderState, RejectReason, Side, TimeInForce,
};
use rusqlite::{Connection, OptionalExtension, params};
use rust_decimal::Decimal;

use crate::StoreError;

/// 把 `Decimal` 序列化为数据库中的字符串形式。
///
/// 用字符串而非 REAL：止盈目标是 bp 级，浮点误差足以翻转结论。
pub fn dec_to_sql(v: Decimal) -> String {
    v.to_string()
}

/// 从数据库字符串还原 `Decimal`。
///
/// 解析失败时报错而不是退回 0——静默把价格变成 0 会产生荒谬的订单。
pub fn dec_from_sql(s: &str) -> Result<Decimal, StoreError> {
    s.parse::<Decimal>()
        .map_err(|_| StoreError::InvalidDecimal(s.to_string()))
}

/// 时间戳毫秒。
pub fn ts_to_sql(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

pub fn ts_from_sql(ms: i64) -> Result<DateTime<Utc>, StoreError> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .ok_or(StoreError::InvalidTimestamp(ms))
}

/// 保存一张订单的当前状态（插入或更新）。
pub fn upsert_order(
    conn: &Connection,
    order: &Order,
    state: &OrderState,
    exchange_id: Option<&str>,
    filled: Decimal,
    avg_price: Option<Decimal>,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let (tif, gtd_deadline) = match order.tif {
        TimeInForce::PostOnly => ("POST_ONLY", None),
        TimeInForce::PostOnlyGtd { deadline } => ("POST_ONLY_GTD", Some(ts_to_sql(deadline))),
    };
    let (state_tag, reject_reason) = state_tag_and_reason(state);

    conn.execute(
        r#"
        INSERT INTO orders (
            client_order_id, exchange_order_id, symbol, purpose, side,
            quantity, limit_price, tif, gtd_deadline_ms, reduce_only, parent_id,
            state, filled_quantity, avg_price, reject_reason, created_at_ms, updated_at_ms
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16)
        ON CONFLICT(client_order_id) DO UPDATE SET
            exchange_order_id = excluded.exchange_order_id,
            state             = excluded.state,
            filled_quantity   = excluded.filled_quantity,
            avg_price         = excluded.avg_price,
            reject_reason     = excluded.reject_reason,
            updated_at_ms     = excluded.updated_at_ms
        "#,
        params![
            order.client_id.as_str(),
            exchange_id,
            order.symbol,
            purpose_tag(order.purpose),
            side_tag(order.side),
            dec_to_sql(order.quantity.get()),
            dec_to_sql(order.limit_price.get()),
            tif,
            gtd_deadline,
            if order.reduce_only() { 1 } else { 0 },
            order.parent.as_ref().map(|p| p.as_str().to_string()),
            state_tag,
            dec_to_sql(filled),
            avg_price.map(dec_to_sql),
            reject_reason,
            ts_to_sql(now),
        ],
    )?;
    Ok(())
}

/// 记录一笔成交。
pub fn insert_fill(conn: &Connection, fill: &Fill) -> Result<bool, StoreError> {
    // `trade_id` 是主键，重复插入直接忽略——同一笔成交可能从用户数据流和
    // 主动查询两条路到达。
    let n = conn.execute(
        r#"
        INSERT OR IGNORE INTO fills (
            trade_id, client_order_id, symbol, quantity, price, fee, fee_asset, filled_at_ms
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
        "#,
        params![
            fill.trade_id,
            fill.client_id.as_str(),
            symbol_of_order(conn, &fill.client_id)?,
            dec_to_sql(fill.quantity.get()),
            dec_to_sql(fill.price.get()),
            dec_to_sql(fill.fee),
            fill.fee_asset,
            ts_to_sql(fill.at),
        ],
    )?;
    Ok(n > 0)
}

fn symbol_of_order(conn: &Connection, id: &ClientOrderId) -> Result<String, StoreError> {
    let s: Option<String> = conn
        .query_row(
            "SELECT symbol FROM orders WHERE client_order_id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    s.ok_or_else(|| StoreError::UnknownOrder(id.to_string()))
}

/// 要写入的持仓内容。
///
/// 独立成类型而不是元组：五个位置参数里有两个是 `Option<Decimal>`，
/// 传错顺序编译器不会拦，而这里的顺序错误会直接把止损价写进入场价。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionUpdate<'a> {
    pub symbol: &'a str,
    pub side: Side,
    pub quantity: Decimal,
    pub entry_price: Decimal,
    pub stop_price: Option<Decimal>,
    pub opened_at: DateTime<Utc>,
}

/// 保存持仓（或删除）。
///
/// `None` 表示已平仓，需要把记录删掉——留着零数量的行会让"当前持仓"查询
/// 返回幽灵记录。
pub fn upsert_position(
    conn: &Connection,
    symbol: &str,
    position: Option<PositionUpdate<'_>>,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    match position {
        None => {
            conn.execute("DELETE FROM positions WHERE symbol = ?1", [symbol])?;
        }
        Some(p) => {
            conn.execute(
                r#"
                INSERT INTO positions (symbol, side, quantity, entry_price, stop_price, opened_at_ms, updated_at_ms)
                VALUES (?1,?2,?3,?4,?5,?6,?7)
                ON CONFLICT(symbol) DO UPDATE SET
                    side          = excluded.side,
                    quantity      = excluded.quantity,
                    entry_price   = excluded.entry_price,
                    stop_price    = excluded.stop_price,
                    updated_at_ms = excluded.updated_at_ms
                "#,
                params![
                    symbol,
                    side_tag(p.side),
                    dec_to_sql(p.quantity),
                    dec_to_sql(p.entry_price),
                    p.stop_price.map(dec_to_sql),
                    ts_to_sql(p.opened_at),
                    ts_to_sql(now),
                ],
            )?;
        }
    }
    Ok(())
}

/// 累加某结算资产的已实现盈亏。
pub fn add_realized_pnl(conn: &Connection, asset: &str, delta: Decimal) -> Result<(), StoreError> {
    let current: Option<String> = conn
        .query_row(
            "SELECT amount FROM realized_pnl WHERE settlement_asset = ?1",
            [asset],
            |r| r.get(0),
        )
        .optional()?;
    let new = match current {
        Some(s) => dec_from_sql(&s)? + delta,
        None => delta,
    };
    conn.execute(
        r#"
        INSERT INTO realized_pnl (settlement_asset, amount) VALUES (?1, ?2)
        ON CONFLICT(settlement_asset) DO UPDATE SET amount = excluded.amount
        "#,
        params![asset, dec_to_sql(new)],
    )?;
    Ok(())
}

pub fn realized_pnl(conn: &Connection, asset: &str) -> Result<Decimal, StoreError> {
    let s: Option<String> = conn
        .query_row(
            "SELECT amount FROM realized_pnl WHERE settlement_asset = ?1",
            [asset],
            |r| r.get(0),
        )
        .optional()?;
    match s {
        Some(v) => dec_from_sql(&v),
        None => Ok(Decimal::ZERO),
    }
}

/// 一张订单的持久化记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderRow {
    pub client_id: ClientOrderId,
    pub symbol: String,
    pub purpose: OrderPurpose,
    pub side: Side,
    pub quantity: Decimal,
    pub limit_price: Decimal,
    pub state_tag: String,
    pub filled: Decimal,
    pub avg_price: Option<Decimal>,
    pub reject_reason: Option<String>,
    pub exchange_id: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl OrderRow {
    /// 该订单是否可能仍在交易所挂着。
    ///
    /// **`UNKNOWN` 也算**——那正是最需要查询对账的状态。进程重启后必须
    /// 把这些查出来，否则会漏掉真实挂单。
    pub fn needs_reconciliation(&self) -> bool {
        matches!(
            self.state_tag.as_str(),
            "LIVE" | "PARTIALLY_FILLED" | "UNKNOWN"
        )
    }
}

/// 所有需要查询对账的订单。
///
/// 启动时与重连后都必须先跑这个列表，然后才允许恢复交易。
pub fn orders_needing_reconciliation(conn: &Connection) -> Result<Vec<OrderRow>, StoreError> {
    let mut stmt = conn.prepare(
        r#"
        SELECT client_order_id, symbol, purpose, side, quantity, limit_price,
               state, filled_quantity, avg_price, reject_reason, exchange_order_id, updated_at_ms
        FROM orders
        WHERE state IN ('LIVE','PARTIALLY_FILLED','UNKNOWN')
        ORDER BY updated_at_ms ASC
        "#,
    )?;
    let rows = stmt
        .query_map([], row_to_order)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 某交易对的当前持仓。
pub fn load_position(conn: &Connection, symbol: &str) -> Result<Option<PositionRow>, StoreError> {
    conn.query_row(
        "SELECT symbol, side, quantity, entry_price, stop_price, opened_at_ms FROM positions WHERE symbol = ?1",
        [symbol],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        },
    )
    .optional()?
    .map(|(symbol, side, qty, entry, stop, opened)| {
        Ok(PositionRow {
            symbol,
            side: side_from_tag(&side)?,
            quantity: dec_from_sql(&qty)?,
            entry_price: dec_from_sql(&entry)?,
            stop_price: stop.as_deref().map(dec_from_sql).transpose()?,
            opened_at: ts_from_sql(opened)?,
        })
    })
    .transpose()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionRow {
    pub symbol: String,
    pub side: Side,
    pub quantity: Decimal,
    pub entry_price: Decimal,
    pub stop_price: Option<Decimal>,
    pub opened_at: DateTime<Utc>,
}

/// 订单历史，按时间倒序。
pub fn recent_orders(
    conn: &Connection,
    symbol: Option<&str>,
    limit: usize,
) -> Result<Vec<OrderRow>, StoreError> {
    let mut sql = String::from(
        r#"
        SELECT client_order_id, symbol, purpose, side, quantity, limit_price,
               state, filled_quantity, avg_price, reject_reason, exchange_order_id, updated_at_ms
        FROM orders
        "#,
    );
    if symbol.is_some() {
        sql.push_str(" WHERE symbol = ?1 ORDER BY updated_at_ms DESC LIMIT ?2");
    } else {
        sql.push_str(" ORDER BY updated_at_ms DESC LIMIT ?1");
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = match symbol {
        Some(s) => stmt
            .query_map(params![s, limit as i64], row_to_order)?
            .collect::<Result<Vec<_>, _>>()?,
        None => stmt
            .query_map(params![limit as i64], row_to_order)?
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(rows)
}

/// 某时间范围内的成交明细（交易总览用）。
pub fn fills_in_range(
    conn: &Connection,
    symbol: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<FillRow>, StoreError> {
    let mut stmt = conn.prepare(
        r#"
        SELECT trade_id, client_order_id, quantity, price, fee, fee_asset, filled_at_ms
        FROM fills
        WHERE symbol = ?1 AND filled_at_ms >= ?2 AND filled_at_ms < ?3
        ORDER BY filled_at_ms ASC
        "#,
    )?;
    let rows = stmt
        .query_map(params![symbol, ts_to_sql(from), ts_to_sql(to)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?
        .map(|x| {
            let (trade_id, order, qty, px, fee, asset, ms) = x?;
            Ok(FillRow {
                trade_id,
                client_id: ClientOrderId(order),
                quantity: dec_from_sql(&qty)?,
                price: dec_from_sql(&px)?,
                fee: dec_from_sql(&fee)?,
                fee_asset: asset,
                at: ts_from_sql(ms)?,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(rows)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FillRow {
    pub trade_id: String,
    pub client_id: ClientOrderId,
    pub quantity: Decimal,
    pub price: Decimal,
    pub fee: Decimal,
    pub fee_asset: String,
    pub at: DateTime<Utc>,
}

fn row_to_order(r: &rusqlite::Row<'_>) -> rusqlite::Result<OrderRow> {
    let client: String = r.get(0)?;
    let purpose: String = r.get(2)?;
    let side: String = r.get(3)?;
    let qty: String = r.get(4)?;
    let px: String = r.get(5)?;
    let filled: String = r.get(7)?;
    let avg: Option<String> = r.get(8)?;
    let ms: i64 = r.get(11)?;

    let map_err = |e: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::InvalidDecimal(e)),
        )
    };

    Ok(OrderRow {
        client_id: ClientOrderId(client),
        symbol: r.get(1)?,
        purpose: match purpose.as_str() {
            "ENTRY" => OrderPurpose::Entry,
            "TAKE_PROFIT" => OrderPurpose::TakeProfit,
            "STOP_LOSS" => OrderPurpose::StopLoss,
            other => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(StoreError::InvalidTag(format!("purpose={other}"))),
                ));
            }
        },
        side: side_from_tag(&side).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        quantity: dec_from_sql(&qty).map_err(|e| map_err(e.to_string()))?,
        limit_price: dec_from_sql(&px).map_err(|e| map_err(e.to_string()))?,
        state_tag: r.get(6)?,
        filled: dec_from_sql(&filled).map_err(|e| map_err(e.to_string()))?,
        avg_price: avg
            .as_deref()
            .map(dec_from_sql)
            .transpose()
            .map_err(|e| map_err(e.to_string()))?,
        reject_reason: r.get(9)?,
        exchange_id: r.get(10)?,
        updated_at: ts_from_sql(ms).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
    })
}

pub fn side_tag(s: Side) -> &'static str {
    match s {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

pub fn side_from_tag(s: &str) -> Result<Side, StoreError> {
    match s {
        "BUY" => Ok(Side::Buy),
        "SELL" => Ok(Side::Sell),
        other => Err(StoreError::InvalidTag(format!("side={other}"))),
    }
}

pub fn purpose_tag(p: OrderPurpose) -> &'static str {
    match p {
        OrderPurpose::Entry => "ENTRY",
        OrderPurpose::TakeProfit => "TAKE_PROFIT",
        OrderPurpose::StopLoss => "STOP_LOSS",
    }
}

/// 订单状态与拒单原因分别存两列：状态是索引字段，原因是可读的诊断信息。
fn state_tag_and_reason(state: &OrderState) -> (&'static str, Option<String>) {
    match state {
        OrderState::PendingSubmit => ("PENDING_SUBMIT", None),
        OrderState::Live => ("LIVE", None),
        OrderState::PartiallyFilled { .. } => ("PARTIALLY_FILLED", None),
        OrderState::Filled { .. } => ("FILLED", None),
        OrderState::Cancelled { .. } => ("CANCELLED", None),
        OrderState::Rejected { reason } => ("REJECTED", Some(format!("{reason:?}"))),
        OrderState::Expired => ("EXPIRED", None),
        OrderState::Unknown { .. } => ("UNKNOWN", None),
    }
}

/// 从持久化标签还原拒单原因。
pub fn reject_reason_from_tag(tag: &str) -> Option<RejectReason> {
    match tag {
        "PostOnlyWouldCross" => Some(RejectReason::PostOnlyWouldCross),
        "InsufficientMargin" => Some(RejectReason::InsufficientMargin),
        "PriceOutOfRange" => Some(RejectReason::PriceOutOfRange),
        "InvalidQuantity" => Some(RejectReason::InvalidQuantity),
        "InstrumentNotTrading" => Some(RejectReason::InstrumentNotTrading),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{Price, Qty};
    use rust_decimal_macros::dec;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::configure(&conn).unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    fn order(id: &str, purpose: OrderPurpose, side: Side) -> Order {
        Order {
            client_id: ClientOrderId(id.into()),
            symbol: "ETHUSDC".into(),
            purpose,
            side,
            quantity: Qty::new(dec!(1)),
            limit_price: Price::new(dec!(3200)),
            tif: TimeInForce::PostOnly,
            parent: None,
        }
    }

    fn fill(trade_id: &str, order_id: &str, qty: Decimal, px: Decimal) -> Fill {
        Fill {
            trade_id: trade_id.into(),
            client_id: ClientOrderId(order_id.into()),
            quantity: Qty::new(qty),
            price: Price::new(px),
            fee: dec!(-0.64),
            fee_asset: "USDC".into(),
            at: Utc::now(),
        }
    }

    #[test]
    fn order_round_trips_with_exact_decimal() {
        let conn = db();
        let o = order("mm:1", OrderPurpose::Entry, Side::Buy);
        let now = Utc::now();
        upsert_order(
            &conn,
            &o,
            &OrderState::Live,
            Some("12345"),
            dec!(0),
            None,
            now,
        )
        .unwrap();

        let rows = recent_orders(&conn, Some("ETHUSDC"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].quantity, dec!(1));
        assert_eq!(rows[0].limit_price, dec!(3200));
        assert_eq!(rows[0].exchange_id.as_deref(), Some("12345"));
        assert_eq!(rows[0].state_tag, "LIVE");
    }

    /// 高精度价格必须无损往返——止盈目标是 bp 级，丢精度会翻转结论。
    #[test]
    fn high_precision_price_survives_round_trip() {
        let conn = db();
        let mut o = order("mm:1", OrderPurpose::Entry, Side::Buy);
        o.limit_price = Price::new(dec!(3200.12345678));
        upsert_order(
            &conn,
            &o,
            &OrderState::Live,
            None,
            dec!(0),
            None,
            Utc::now(),
        )
        .unwrap();

        let rows = recent_orders(&conn, None, 10).unwrap();
        assert_eq!(rows[0].limit_price, dec!(3200.12345678));
    }

    #[test]
    fn same_order_id_updates_in_place() {
        let conn = db();
        let o = order("mm:1", OrderPurpose::Entry, Side::Buy);
        let now = Utc::now();
        upsert_order(
            &conn,
            &o,
            &OrderState::PendingSubmit,
            None,
            dec!(0),
            None,
            now,
        )
        .unwrap();
        upsert_order(&conn, &o, &OrderState::Live, Some("E1"), dec!(0), None, now).unwrap();
        upsert_order(
            &conn,
            &o,
            &OrderState::Filled {
                filled: Qty::new(dec!(1)),
                avg: Price::new(dec!(3200)),
            },
            Some("E1"),
            dec!(1),
            Some(dec!(3200)),
            now,
        )
        .unwrap();

        let rows = recent_orders(&conn, None, 10).unwrap();
        assert_eq!(rows.len(), 1, "同一订单 ID 不应产生多行");
        assert_eq!(rows[0].state_tag, "FILLED");
        assert_eq!(rows[0].filled, dec!(1));
    }

    /// 重启后必须能找出所有可能仍在交易所挂着的订单。
    /// `UNKNOWN` 尤其重要——那是最需要查询对账的状态。
    #[test]
    fn reconciliation_list_includes_unknown_state() {
        let conn = db();
        let now = Utc::now();
        upsert_order(
            &conn,
            &order("a", OrderPurpose::Entry, Side::Buy),
            &OrderState::Live,
            None,
            dec!(0),
            None,
            now,
        )
        .unwrap();
        upsert_order(
            &conn,
            &order("b", OrderPurpose::Entry, Side::Buy),
            &OrderState::Unknown {
                since: now,
                last_probe: None,
            },
            None,
            dec!(0),
            None,
            now,
        )
        .unwrap();
        upsert_order(
            &conn,
            &order("c", OrderPurpose::Entry, Side::Buy),
            &OrderState::Filled {
                filled: Qty::new(dec!(1)),
                avg: Price::new(dec!(3200)),
            },
            None,
            dec!(1),
            None,
            now,
        )
        .unwrap();
        upsert_order(
            &conn,
            &order("d", OrderPurpose::Entry, Side::Buy),
            &OrderState::Rejected {
                reason: RejectReason::PostOnlyWouldCross,
            },
            None,
            dec!(0),
            None,
            now,
        )
        .unwrap();

        let pending = orders_needing_reconciliation(&conn).unwrap();
        let ids: Vec<&str> = pending.iter().map(|r| r.client_id.as_str()).collect();
        assert!(ids.contains(&"a"), "LIVE 需要查询");
        assert!(ids.contains(&"b"), "UNKNOWN 需要查询——这是最容易漏的");
        assert!(!ids.contains(&"c"), "已成交不需要查询");
        assert!(!ids.contains(&"d"), "已拒绝不需要查询");
        assert_eq!(pending.len(), 2);
    }

    /// 重复的成交 ID 必须被忽略——同一笔成交可能从两条路到达。
    #[test]
    fn duplicate_fill_trade_id_is_ignored() {
        let conn = db();
        let o = order("mm:1", OrderPurpose::Entry, Side::Buy);
        upsert_order(
            &conn,
            &o,
            &OrderState::Live,
            None,
            dec!(0),
            None,
            Utc::now(),
        )
        .unwrap();

        let f = fill("T1", "mm:1", dec!(1), dec!(3200));
        assert!(insert_fill(&conn, &f).unwrap(), "首次插入应成功");
        assert!(!insert_fill(&conn, &f).unwrap(), "重复插入应被忽略");

        let rows = fills_in_range(
            &conn,
            "ETHUSDC",
            Utc::now() - chrono::Duration::days(1),
            Utc::now() + chrono::Duration::days(1),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    /// 成交必须关联到已存在的订单——否则会留下孤儿记录。
    #[test]
    fn fill_without_order_is_rejected() {
        let conn = db();
        let r = insert_fill(&conn, &fill("T1", "nonexistent", dec!(1), dec!(3200)));
        assert!(r.is_err());
    }

    #[test]
    fn position_round_trips_and_clears() {
        let conn = db();
        let now = Utc::now();
        upsert_position(
            &conn,
            "ETHUSDC",
            Some(PositionUpdate {
                symbol: "ETHUSDC",
                side: Side::Buy,
                quantity: dec!(1.5),
                entry_price: dec!(3200),
                stop_price: Some(dec!(3190)),
                opened_at: now,
            }),
            now,
        )
        .unwrap();

        let p = load_position(&conn, "ETHUSDC").unwrap().expect("应有持仓");
        assert_eq!(p.quantity, dec!(1.5));
        assert_eq!(p.entry_price, dec!(3200));
        assert_eq!(p.stop_price, Some(dec!(3190)));

        upsert_position(&conn, "ETHUSDC", None, now).unwrap();
        assert!(
            load_position(&conn, "ETHUSDC").unwrap().is_none(),
            "平仓后不应留幽灵记录"
        );
    }

    /// USDT 与 USDC 的已实现盈亏必须分开累加，绝不能相加。
    #[test]
    fn realized_pnl_keeps_settlement_assets_separate() {
        let conn = db();
        add_realized_pnl(&conn, "USDC", dec!(-1.5)).unwrap();
        add_realized_pnl(&conn, "USDT", dec!(3.0)).unwrap();
        add_realized_pnl(&conn, "USDC", dec!(0.5)).unwrap();

        assert_eq!(realized_pnl(&conn, "USDC").unwrap(), dec!(-1.0));
        assert_eq!(realized_pnl(&conn, "USDT").unwrap(), dec!(3.0));
        assert_eq!(realized_pnl(&conn, "BNB").unwrap(), Decimal::ZERO);
    }

    /// 非法 Decimal 必须报错而不是静默变成 0。
    #[test]
    fn invalid_decimal_is_an_error_not_zero() {
        assert!(dec_from_sql("not-a-number").is_err());
        assert_eq!(dec_from_sql("0").unwrap(), Decimal::ZERO);
    }

    #[test]
    fn unknown_side_tag_is_an_error() {
        assert!(side_from_tag("SIDEWAYS").is_err());
        assert_eq!(side_from_tag("BUY").unwrap(), Side::Buy);
    }

    #[test]
    fn gtd_deadline_is_persisted() {
        let conn = db();
        let deadline = Utc::now() + chrono::Duration::minutes(2);
        let mut o = order("mm:1", OrderPurpose::Entry, Side::Buy);
        o.tif = TimeInForce::PostOnlyGtd { deadline };
        upsert_order(
            &conn,
            &o,
            &OrderState::Live,
            None,
            dec!(0),
            None,
            Utc::now(),
        )
        .unwrap();

        let stored: Option<i64> = conn
            .query_row(
                "SELECT gtd_deadline_ms FROM orders WHERE client_order_id='mm:1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored.unwrap(), ts_to_sql(deadline));
    }

    #[test]
    fn rejection_reason_is_persisted_for_diagnosis() {
        let conn = db();
        upsert_order(
            &conn,
            &order("mm:1", OrderPurpose::Entry, Side::Buy),
            &OrderState::Rejected {
                reason: RejectReason::PostOnlyWouldCross,
            },
            None,
            dec!(0),
            None,
            Utc::now(),
        )
        .unwrap();

        let rows = recent_orders(&conn, None, 10).unwrap();
        assert_eq!(rows[0].state_tag, "REJECTED");
        let reason = rows[0].reject_reason.as_deref().unwrap();
        assert_eq!(
            reject_reason_from_tag(reason),
            Some(RejectReason::PostOnlyWouldCross)
        );
    }

    #[test]
    fn orders_are_returned_newest_first() {
        let conn = db();
        let t0 = Utc::now();
        upsert_order(
            &conn,
            &order("old", OrderPurpose::Entry, Side::Buy),
            &OrderState::Live,
            None,
            dec!(0),
            None,
            t0,
        )
        .unwrap();
        upsert_order(
            &conn,
            &order("new", OrderPurpose::Entry, Side::Buy),
            &OrderState::Live,
            None,
            dec!(0),
            None,
            t0 + chrono::Duration::seconds(10),
        )
        .unwrap();

        let rows = recent_orders(&conn, None, 10).unwrap();
        assert_eq!(rows[0].client_id.as_str(), "new");
        assert_eq!(rows[1].client_id.as_str(), "old");
    }

    #[test]
    fn fills_in_range_filters_by_time() {
        let conn = db();
        upsert_order(
            &conn,
            &order("mm:1", OrderPurpose::Entry, Side::Buy),
            &OrderState::Live,
            None,
            dec!(0),
            None,
            Utc::now(),
        )
        .unwrap();

        let t0 = Utc::now();
        let mut f1 = fill("T1", "mm:1", dec!(1), dec!(3200));
        f1.at = t0;
        let mut f2 = fill("T2", "mm:1", dec!(1), dec!(3210));
        f2.at = t0 + chrono::Duration::hours(5);
        insert_fill(&conn, &f1).unwrap();
        insert_fill(&conn, &f2).unwrap();

        let rows = fills_in_range(
            &conn,
            "ETHUSDC",
            t0 - chrono::Duration::minutes(1),
            t0 + chrono::Duration::hours(1),
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "只应返回范围内的成交");
        assert_eq!(rows[0].trade_id, "T1");
    }

    #[test]
    fn order_history_can_be_filtered_by_symbol() {
        let conn = db();
        let now = Utc::now();
        let mut a = order("a", OrderPurpose::Entry, Side::Buy);
        a.symbol = "ETHUSDC".into();
        let mut b = order("b", OrderPurpose::Entry, Side::Buy);
        b.symbol = "BTCUSDC".into();
        upsert_order(&conn, &a, &OrderState::Live, None, dec!(0), None, now).unwrap();
        upsert_order(&conn, &b, &OrderState::Live, None, dec!(0), None, now).unwrap();

        assert_eq!(recent_orders(&conn, Some("ETHUSDC"), 10).unwrap().len(), 1);
        assert_eq!(recent_orders(&conn, None, 10).unwrap().len(), 2);
    }
}
