//! 回测记录、AI 会话、图表标注的持久化。
//!
//! # 三类历史的共同点
//!
//! 它们都是**只追加、按时间查询**的数据，与订单那种"必须能更新单行状态"
//! 的语义不同。所以这里用 `INSERT`（订单用 `upsert`），查询一律带时间索引。
//!
//! # 回测记录必须存结论可信度
//!
//! `sign_flips`、`breakeven_fill_rate`、`adverse_markout_5s`、`fee_incomplete`
//! 这几个字段不是可选元信息——它们决定这个结果**能不能用于决策**。回测跑完
//! 不记录它们，等于把一份无法判断可信度的数字存进了历史，几周后回看时完全
//! 不知道当时是否依赖了不现实的成交假设。

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use rust_decimal::Decimal;

use crate::StoreError;
use crate::orders::{dec_from_sql, dec_to_sql, ts_from_sql, ts_to_sql};

/// 一条回测记录。写入后不可变。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BacktestRunRow {
    pub run_id: String,
    pub symbol: String,
    pub strategy_id: String,
    /// 策略参数快照（JSON）。回测必须可复现，所以参数要落库。
    pub strategy_params: String,
    pub fill_model: String,
    pub initial_equity: Decimal,
    pub final_equity: Decimal,
    pub trade_count: i64,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// 乐观模型与诚实模型结论方向相反。
    pub sign_flips: bool,
    /// 盈亏平衡成交率。`None` 表示乐观模型本身不盈利。
    pub breakeven_fill_rate: Option<Decimal>,
    /// 5 秒 markout 均值。负数 = 系统性逆向选择。
    pub adverse_markout_5s: Option<Decimal>,
    /// 费率来源非权威，结论不完整。
    pub fee_incomplete: bool,
    pub gap_count: i64,
}

impl BacktestRunRow {
    /// 这份结果是否可用于决策。
    ///
    /// 三个否决条件，任一成立就不能拿它下结论：
    /// 符号翻转、系统性逆向选择、费率来源不可信。
    pub fn is_actionable(&self) -> bool {
        !self.sign_flips
            && !self.fee_incomplete
            && !self.adverse_markout_5s.is_some_and(|m| m < Decimal::ZERO)
            && self
                .breakeven_fill_rate
                .is_some_and(|r| r < Decimal::new(8, 1))
    }

    /// 面向用户的简短结论。
    pub fn verdict(&self) -> &'static str {
        if self.sign_flips {
            "结论不可信：乐观与诚实模型方向相反"
        } else if self.fee_incomplete {
            "结论不完整：费率来源未经交易所对账"
        } else if self.adverse_markout_5s.is_some_and(|m| m < Decimal::ZERO) {
            "存在系统性逆向选择"
        } else if self.breakeven_fill_rate.is_none() {
            "乐观模型下即不盈利，策略没有 edge"
        } else if self
            .breakeven_fill_rate
            .is_some_and(|r| r >= Decimal::new(8, 1))
        {
            "成交率赌注：需要乐观模型 80% 以上的成交量才能不亏"
        } else {
            "结论相对稳健"
        }
    }
}

pub fn insert_backtest_run(conn: &Connection, row: &BacktestRunRow) -> Result<(), StoreError> {
    conn.execute(
        r#"
        INSERT INTO backtest_runs (
            run_id, symbol, strategy_id, strategy_params, fill_model,
            initial_equity, final_equity, trade_count, start_ms, end_ms,
            sign_flips, breakeven_fill_rate, adverse_markout_5s,
            fee_incomplete, gap_count, created_at_ms
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
        "#,
        params![
            row.run_id,
            row.symbol,
            row.strategy_id,
            row.strategy_params,
            row.fill_model,
            dec_to_sql(row.initial_equity),
            dec_to_sql(row.final_equity),
            row.trade_count,
            ts_to_sql(row.start),
            ts_to_sql(row.end),
            if row.sign_flips { 1 } else { 0 },
            row.breakeven_fill_rate.map(dec_to_sql),
            row.adverse_markout_5s.map(dec_to_sql),
            if row.fee_incomplete { 1 } else { 0 },
            row.gap_count,
            ts_to_sql(Utc::now()),
        ],
    )?;
    Ok(())
}

/// 某个交易对的回测历史，最新在前。
pub fn recent_backtest_runs(
    conn: &Connection,
    symbol: Option<&str>,
    limit: usize,
) -> Result<Vec<BacktestRunRow>, StoreError> {
    let mut sql = String::from(
        r#"
        SELECT run_id, symbol, strategy_id, strategy_params, fill_model,
               initial_equity, final_equity, trade_count, start_ms, end_ms,
               sign_flips, breakeven_fill_rate, adverse_markout_5s,
               fee_incomplete, gap_count
        FROM backtest_runs
        "#,
    );
    if symbol.is_some() {
        sql.push_str(" WHERE symbol = ?1 ORDER BY created_at_ms DESC LIMIT ?2");
    } else {
        sql.push_str(" ORDER BY created_at_ms DESC LIMIT ?1");
    }

    let mut stmt = conn.prepare(&sql)?;
    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<BacktestRunRow> {
        let bad = |e: String| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(StoreError::InvalidDecimal(e)),
            )
        };
        let initial: String = r.get(5)?;
        let fin: String = r.get(6)?;
        let be: Option<String> = r.get(11)?;
        let mo: Option<String> = r.get(12)?;
        Ok(BacktestRunRow {
            run_id: r.get(0)?,
            symbol: r.get(1)?,
            strategy_id: r.get(2)?,
            strategy_params: r.get(3)?,
            fill_model: r.get(4)?,
            initial_equity: dec_from_sql(&initial).map_err(|e| bad(e.to_string()))?,
            final_equity: dec_from_sql(&fin).map_err(|e| bad(e.to_string()))?,
            trade_count: r.get(7)?,
            start: ts_from_sql(r.get(8)?).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
            end: ts_from_sql(r.get(9)?).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
            sign_flips: r.get::<_, i64>(10)? != 0,
            breakeven_fill_rate: be
                .as_deref()
                .map(dec_from_sql)
                .transpose()
                .map_err(|e| bad(e.to_string()))?,
            adverse_markout_5s: mo
                .as_deref()
                .map(dec_from_sql)
                .transpose()
                .map_err(|e| bad(e.to_string()))?,
            fee_incomplete: r.get::<_, i64>(13)? != 0,
            gap_count: r.get(14)?,
        })
    };

    let rows = match symbol {
        Some(s) => stmt
            .query_map(params![s, limit as i64], map)?
            .collect::<Result<Vec<_>, _>>()?,
        None => stmt
            .query_map(params![limit as i64], map)?
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(rows)
}

pub fn get_backtest_run(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<BacktestRunRow>, StoreError> {
    let mut all = recent_backtest_runs(conn, None, 10_000)?;
    Ok(all.drain(..).find(|r| r.run_id == run_id))
}

/// 一次 AI 分析会话。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiSessionRow {
    pub symbol: String,
    pub interval: String,
    pub question: String,
    pub answer: String,
    /// 模型产出的结构化标注（Drawing JSON 数组）。
    ///
    /// 与用户手画的线是**同一套类型**，所以可以直接渲染到图上，也可以
    /// 作为下一次提问的上下文。
    pub annotations: String,
    /// 来源：`deepseek` 或 `local-deterministic`（离线降级）。
    pub source: String,
    pub model: Option<String>,
    /// 提问时的上下文快照（指标、持仓、策略版本）。
    pub context_json: String,
}

pub fn insert_ai_session(conn: &Connection, row: &AiSessionRow) -> Result<i64, StoreError> {
    conn.execute(
        r#"
        INSERT INTO ai_sessions (
            symbol, interval, question, answer, annotations, source, model, context_json, created_at_ms
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
        "#,
        params![
            row.symbol,
            row.interval,
            row.question,
            row.answer,
            row.annotations,
            row.source,
            row.model,
            row.context_json,
            ts_to_sql(Utc::now()),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 某交易对的 AI 会话历史，最新在前。
///
/// 返回 `(id, 提问, 回答, 标注, 来源, 时刻)`。只返回展示需要的字段——
/// 完整上下文可能很大，按需再取。
pub fn recent_ai_sessions(
    conn: &Connection,
    symbol: &str,
    limit: usize,
) -> Result<Vec<AiSessionRow>, StoreError> {
    let mut stmt = conn.prepare(
        r#"
        SELECT symbol, interval, question, answer, annotations, source, model, context_json
        FROM ai_sessions
        WHERE symbol = ?1
        ORDER BY created_at_ms DESC
        LIMIT ?2
        "#,
    )?;
    let rows = stmt
        .query_map(params![symbol, limit as i64], |r| {
            Ok(AiSessionRow {
                symbol: r.get(0)?,
                interval: r.get(1)?,
                question: r.get(2)?,
                answer: r.get(3)?,
                annotations: r.get(4)?,
                source: r.get(5)?,
                model: r.get(6)?,
                context_json: r.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 一条图表标注。用户手画与 AI 产出共用同一套类型，只有 `source` 不同。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnotationRow {
    pub id: String,
    pub symbol: String,
    pub interval: String,
    /// 标注种类：`trendLine` / `horizontalLine` / `entryLevel` 等。
    pub kind: String,
    /// `user` / `ai` / `strategy`。这是审计轨迹与区分渲染的依据。
    pub source: String,
    pub payload_json: String,
    pub label: Option<String>,
    pub visible: bool,
    pub locked: bool,
}

pub fn upsert_annotation(conn: &Connection, row: &AnnotationRow) -> Result<(), StoreError> {
    conn.execute(
        r#"
        INSERT INTO annotations (id, symbol, interval, kind, source, payload_json, label, visible, locked, created_at_ms)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
        ON CONFLICT(id) DO UPDATE SET
            payload_json = excluded.payload_json,
            label        = excluded.label,
            visible      = excluded.visible,
            locked       = excluded.locked
        "#,
        params![
            row.id,
            row.symbol,
            row.interval,
            row.kind,
            row.source,
            row.payload_json,
            row.label,
            if row.visible { 1 } else { 0 },
            if row.locked { 1 } else { 0 },
            ts_to_sql(Utc::now()),
        ],
    )?;
    Ok(())
}

pub fn list_annotations(
    conn: &Connection,
    symbol: &str,
    interval: &str,
) -> Result<Vec<AnnotationRow>, StoreError> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, symbol, interval, kind, source, payload_json, label, visible, locked
        FROM annotations
        WHERE symbol = ?1 AND interval = ?2
        ORDER BY created_at_ms ASC
        "#,
    )?;
    let rows = stmt
        .query_map(params![symbol, interval], |r| {
            Ok(AnnotationRow {
                id: r.get(0)?,
                symbol: r.get(1)?,
                interval: r.get(2)?,
                kind: r.get(3)?,
                source: r.get(4)?,
                payload_json: r.get(5)?,
                label: r.get(6)?,
                visible: r.get::<_, i64>(7)? != 0,
                locked: r.get::<_, i64>(8)? != 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn delete_annotation(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    let n = conn.execute("DELETE FROM annotations WHERE id = ?1", [id])?;
    Ok(n > 0)
}

/// 交易总览的聚合统计。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PnlSummary {
    /// 每个结算资产的已实现盈亏。
    pub realized_by_asset: Vec<(String, Decimal)>,
    /// 成交笔数。
    pub fill_count: i64,
    /// 累计手续费，按资产。
    pub fees_by_asset: Vec<(String, Decimal)>,
}

/// 汇总某时间范围的盈亏与费用。
///
/// 手续费**逐行读出用 `Decimal` 相加**，不用 SQL 的 `SUM()`——`fee` 列是
/// 字符串，`CAST(fee AS REAL)` 会引入浮点误差，而手续费要累加到盈亏里，
/// 误差会累积。
pub fn pnl_summary(
    conn: &Connection,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<PnlSummary, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT fee, fee_asset FROM fills WHERE filled_at_ms >= ?1 AND filled_at_ms < ?2",
    )?;
    let rows = stmt.query_map(params![ts_to_sql(from), ts_to_sql(to)], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;

    let mut fees: std::collections::BTreeMap<String, Decimal> = Default::default();
    let mut fill_count = 0i64;
    for row in rows {
        let (fee, asset) = row?;
        *fees.entry(asset).or_insert(Decimal::ZERO) += dec_from_sql(&fee)?;
        fill_count += 1;
    }

    let mut stmt2 = conn.prepare("SELECT settlement_asset, amount FROM realized_pnl")?;
    let realized_by_asset = stmt2
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .map(|x| {
            let (a, v) = x?;
            Ok((a, dec_from_sql(&v)?))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;

    Ok(PnlSummary {
        realized_by_asset,
        fill_count,
        fees_by_asset: fees.into_iter().collect(),
    })
}

/// 某交易对是否有未完成的订单需要处理。
pub fn has_open_orders(conn: &Connection, symbol: &str) -> Result<bool, StoreError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM orders WHERE symbol = ?1 AND state IN ('LIVE','PARTIALLY_FILLED','UNKNOWN')",
        [symbol],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 数据库整体健康检查：版本正确、无孤儿成交、无异常持仓。
///
/// 启动时调用，问题必须暴露而不是静默继续。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityReport {
    pub schema_version: i32,
    pub orphan_fills: i64,
    pub unknown_state_orders: i64,
    pub positions: i64,
}

pub fn integrity_check(conn: &Connection) -> Result<IntegrityReport, StoreError> {
    let schema_version = crate::schema::version(conn)?;
    let orphan_fills: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fills f LEFT JOIN orders o ON f.client_order_id = o.client_order_id WHERE o.client_order_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    let unknown_state_orders: i64 = conn.query_row(
        "SELECT COUNT(*) FROM orders WHERE state = 'UNKNOWN'",
        [],
        |r| r.get(0),
    )?;
    let positions: i64 = conn.query_row("SELECT COUNT(*) FROM positions", [], |r| r.get(0))?;

    Ok(IntegrityReport {
        schema_version,
        orphan_fills,
        unknown_state_orders,
        positions,
    })
}

/// 某个 AI 会话的完整上下文。按需取，因为可能很大。
pub fn ai_session_context(conn: &Connection, id: i64) -> Result<Option<String>, StoreError> {
    let v: Option<String> = conn
        .query_row(
            "SELECT context_json FROM ai_sessions WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v)
}

/// 清理早于给定时点的回测记录（数据维护用）。
pub fn prune_backtest_runs(conn: &Connection, before: DateTime<Utc>) -> Result<usize, StoreError> {
    let n = conn.execute(
        "DELETE FROM backtest_runs WHERE created_at_ms < ?1",
        [ts_to_sql(before)],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::configure(&conn).unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    fn run(
        id: &str,
        sign_flips: bool,
        breakeven: Option<Decimal>,
        markout: Option<Decimal>,
        fee_incomplete: bool,
    ) -> BacktestRunRow {
        let t = Utc::now();
        BacktestRunRow {
            run_id: id.into(),
            symbol: "ETHUSDC".into(),
            strategy_id: "retest".into(),
            strategy_params: r#"{"lookback":60}"#.into(),
            fill_model: "M1_trade_through_queue".into(),
            initial_equity: dec!(10000),
            final_equity: dec!(10500),
            trade_count: 42,
            start: t,
            end: t + chrono::Duration::days(30),
            sign_flips,
            breakeven_fill_rate: breakeven,
            adverse_markout_5s: markout,
            fee_incomplete,
            gap_count: 0,
        }
    }

    #[test]
    fn backtest_run_round_trips() {
        let conn = db();
        let r = run("run-1", false, Some(dec!(0.45)), Some(dec!(0.3)), false);
        insert_backtest_run(&conn, &r).unwrap();

        let got = get_backtest_run(&conn, "run-1").unwrap().expect("应能查到");
        assert_eq!(got.strategy_id, "retest");
        assert_eq!(got.final_equity, dec!(10500));
        assert_eq!(got.breakeven_fill_rate, Some(dec!(0.45)));
        assert_eq!(got.trade_count, 42);
        assert!(!got.sign_flips);
    }

    /// 结论可信度是回测记录的核心价值，必须无损往返。
    #[test]
    fn conclusion_confidence_fields_survive_round_trip() {
        let conn = db();
        insert_backtest_run(&conn, &run("a", true, None, Some(dec!(-1.5)), true)).unwrap();
        let got = get_backtest_run(&conn, "a").unwrap().unwrap();
        assert!(got.sign_flips);
        assert_eq!(got.breakeven_fill_rate, None);
        assert_eq!(got.adverse_markout_5s, Some(dec!(-1.5)));
        assert!(got.fee_incomplete);
        assert!(!got.is_actionable());
        assert!(got.verdict().contains("不可信"));
    }

    /// 三种否决条件各自都能拦住结论。
    #[test]
    fn each_veto_condition_blocks_actionable_verdict() {
        // 正常
        let good = run("g", false, Some(dec!(0.4)), Some(dec!(0.5)), false);
        assert!(good.is_actionable(), "{}", good.verdict());

        // 符号翻转
        let flipped = run("f", true, Some(dec!(0.4)), Some(dec!(0.5)), false);
        assert!(!flipped.is_actionable());
        assert!(flipped.verdict().contains("不可信"));

        // 费率不完整
        let unfee = run("u", false, Some(dec!(0.4)), Some(dec!(0.5)), true);
        assert!(!unfee.is_actionable());
        assert!(unfee.verdict().contains("费率"));

        // 逆向选择
        let adverse = run("m", false, Some(dec!(0.4)), Some(dec!(-0.2)), false);
        assert!(!adverse.is_actionable());
        assert!(adverse.verdict().contains("逆向选择"));

        // 成交率赌注
        let bet = run("b", false, Some(dec!(0.95)), Some(dec!(0.5)), false);
        assert!(!bet.is_actionable());
        assert!(bet.verdict().contains("成交率赌注"));

        // 乐观模型不盈利
        let noedge = run("n", false, None, Some(dec!(0.5)), false);
        assert!(!noedge.is_actionable());
        assert!(noedge.verdict().contains("没有 edge"));
    }

    #[test]
    fn runs_are_listed_newest_first() {
        let conn = db();
        insert_backtest_run(&conn, &run("first", false, None, None, false)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        insert_backtest_run(&conn, &run("second", false, None, None, false)).unwrap();

        let list = recent_backtest_runs(&conn, Some("ETHUSDC"), 10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].run_id, "second");
    }

    #[test]
    fn ai_session_round_trips_with_annotations() {
        let conn = db();
        let row = AiSessionRow {
            symbol: "ETHUSDC".into(),
            interval: "15m".into(),
            question: "现在偏多还是偏空？".into(),
            answer: "当前处于区间震荡，偏向做空失败突破。".into(),
            annotations:
                r#"[{"kind":"trendLine","a":{"time":"2026-08-01T00:00:00Z","price":"3200"}}]"#
                    .into(),
            source: "deepseek".into(),
            model: Some("deepseek-chat".into()),
            context_json: r#"{"strategy":"retest","position":null}"#.into(),
        };
        let id = insert_ai_session(&conn, &row).unwrap();
        assert!(id > 0);

        let list = recent_ai_sessions(&conn, "ETHUSDC", 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].question, "现在偏多还是偏空？");
        assert!(
            list[0].annotations.contains("trendLine"),
            "结构化标注必须保存"
        );
        assert_eq!(list[0].source, "deepseek");

        let ctx = ai_session_context(&conn, id).unwrap().unwrap();
        assert!(ctx.contains("retest"));
    }

    /// 离线降级（无 API Key）的会话也要能存——它同样是复盘材料。
    #[test]
    fn deterministic_fallback_sessions_are_stored() {
        let conn = db();
        let row = AiSessionRow {
            symbol: "ETHUSDC".into(),
            interval: "1h".into(),
            question: "趋势？".into(),
            answer: "本地确定性分析：检测到 3 个枢轴。".into(),
            annotations: "[]".into(),
            source: "local-deterministic".into(),
            model: None,
            context_json: "{}".into(),
        };
        insert_ai_session(&conn, &row).unwrap();
        let list = recent_ai_sessions(&conn, "ETHUSDC", 10).unwrap();
        assert_eq!(list[0].source, "local-deterministic");
        assert_eq!(list[0].model, None);
    }

    #[test]
    fn annotation_upsert_and_delete() {
        let conn = db();
        let a = AnnotationRow {
            id: "d1".into(),
            symbol: "ETHUSDC".into(),
            interval: "15m".into(),
            kind: "trendLine".into(),
            source: "user".into(),
            payload_json: r#"{"a":{"time":"t","price":"3200"}}"#.into(),
            label: Some("下降趋势线".into()),
            visible: true,
            locked: false,
        };
        upsert_annotation(&conn, &a).unwrap();

        // 更新同一 ID
        let mut a2 = a.clone();
        a2.label = Some("已修正".into());
        a2.visible = false;
        upsert_annotation(&conn, &a2).unwrap();

        let list = list_annotations(&conn, "ETHUSDC", "15m").unwrap();
        assert_eq!(list.len(), 1, "同 ID 应更新而非新增");
        assert_eq!(list[0].label.as_deref(), Some("已修正"));
        assert!(!list[0].visible);

        assert!(delete_annotation(&conn, "d1").unwrap());
        assert!(
            list_annotations(&conn, "ETHUSDC", "15m")
                .unwrap()
                .is_empty()
        );
        assert!(
            !delete_annotation(&conn, "d1").unwrap(),
            "重复删除应返回 false"
        );
    }

    /// source 必须保留——它是审计轨迹，区分用户手画与 AI 产出。
    #[test]
    fn annotation_source_is_preserved() {
        let conn = db();
        for (i, src) in ["user", "ai", "strategy"].iter().enumerate() {
            upsert_annotation(
                &conn,
                &AnnotationRow {
                    id: format!("d{i}"),
                    symbol: "ETHUSDC".into(),
                    interval: "1h".into(),
                    kind: "horizontalLine".into(),
                    source: (*src).into(),
                    payload_json: "{}".into(),
                    label: None,
                    visible: true,
                    locked: false,
                },
            )
            .unwrap();
        }
        let list = list_annotations(&conn, "ETHUSDC", "1h").unwrap();
        let sources: Vec<&str> = list.iter().map(|a| a.source.as_str()).collect();
        assert!(sources.contains(&"user"));
        assert!(sources.contains(&"ai"));
        assert!(sources.contains(&"strategy"));
    }

    #[test]
    fn annotations_are_scoped_by_symbol_and_interval() {
        let conn = db();
        for (i, (sym, iv)) in [("ETHUSDC", "15m"), ("ETHUSDC", "1h"), ("BTCUSDC", "15m")]
            .iter()
            .enumerate()
        {
            upsert_annotation(
                &conn,
                &AnnotationRow {
                    id: format!("x{i}"),
                    symbol: (*sym).into(),
                    interval: (*iv).into(),
                    kind: "trendLine".into(),
                    source: "user".into(),
                    payload_json: "{}".into(),
                    label: None,
                    visible: true,
                    locked: false,
                },
            )
            .unwrap();
        }
        assert_eq!(list_annotations(&conn, "ETHUSDC", "15m").unwrap().len(), 1);
        assert_eq!(list_annotations(&conn, "ETHUSDC", "1h").unwrap().len(), 1);
        assert_eq!(list_annotations(&conn, "BTCUSDC", "15m").unwrap().len(), 1);
    }

    #[test]
    fn open_orders_detection() {
        let conn = db();
        use domain::{ClientOrderId, Order, OrderPurpose, Price, Qty, Side, TimeInForce};
        let mk = |id: &str| Order {
            client_id: ClientOrderId(id.into()),
            symbol: "ETHUSDC".into(),
            purpose: OrderPurpose::Entry,
            side: Side::Buy,
            quantity: Qty::new(dec!(1)),
            limit_price: Price::new(dec!(3200)),
            tif: TimeInForce::PostOnly,
            parent: None,
        };
        assert!(!has_open_orders(&conn, "ETHUSDC").unwrap());

        crate::orders::upsert_order(
            &conn,
            &mk("a"),
            &domain::OrderState::Live,
            None,
            dec!(0),
            None,
            Utc::now(),
        )
        .unwrap();
        assert!(has_open_orders(&conn, "ETHUSDC").unwrap());

        crate::orders::upsert_order(
            &conn,
            &mk("a"),
            &domain::OrderState::Filled {
                filled: Qty::new(dec!(1)),
                avg: Price::new(dec!(3200)),
            },
            None,
            dec!(1),
            None,
            Utc::now(),
        )
        .unwrap();
        assert!(
            !has_open_orders(&conn, "ETHUSDC").unwrap(),
            "成交后不应再有未完成订单"
        );
    }

    #[test]
    fn integrity_check_reports_clean_database() {
        let conn = db();
        let r = integrity_check(&conn).unwrap();
        assert_eq!(r.schema_version, crate::CURRENT_VERSION);
        assert_eq!(r.orphan_fills, 0);
        assert_eq!(r.unknown_state_orders, 0);
        assert_eq!(r.positions, 0);
    }

    /// 未知状态订单数要能被健康检查发现——它们是启动时必须处理的。
    #[test]
    fn integrity_check_counts_unknown_state_orders() {
        let conn = db();
        use domain::{ClientOrderId, Order, OrderPurpose, Price, Qty, Side, TimeInForce};
        let o = Order {
            client_id: ClientOrderId("x".into()),
            symbol: "ETHUSDC".into(),
            purpose: OrderPurpose::Entry,
            side: Side::Buy,
            quantity: Qty::new(dec!(1)),
            limit_price: Price::new(dec!(3200)),
            tif: TimeInForce::PostOnly,
            parent: None,
        };
        let now = Utc::now();
        crate::orders::upsert_order(
            &conn,
            &o,
            &domain::OrderState::Unknown {
                since: now,
                last_probe: None,
            },
            None,
            dec!(0),
            None,
            now,
        )
        .unwrap();

        let r = integrity_check(&conn).unwrap();
        assert_eq!(r.unknown_state_orders, 1);
    }

    #[test]
    fn prune_removes_old_runs_only() {
        let conn = db();
        insert_backtest_run(&conn, &run("keep", false, None, None, false)).unwrap();
        // 手工把一条记录的时间改到很久以前
        conn.execute(
            "UPDATE backtest_runs SET created_at_ms = ?1 WHERE run_id = 'keep'",
            [ts_to_sql(Utc::now() - chrono::Duration::days(400))],
        )
        .unwrap();
        insert_backtest_run(&conn, &run("new", false, None, None, false)).unwrap();

        let removed = prune_backtest_runs(&conn, Utc::now() - chrono::Duration::days(90)).unwrap();
        assert_eq!(removed, 1);
        let left = recent_backtest_runs(&conn, None, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].run_id, "new");
    }
}
