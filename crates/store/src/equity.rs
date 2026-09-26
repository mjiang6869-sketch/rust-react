//! 模拟盘账户权益的低频快照。每次服务启动是独立 session，不能把重启后的
//! 10,000 初始权益接在上一次运行的曲线上。

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use rust_decimal::Decimal;

use crate::StoreError;
use crate::orders::{dec_from_sql, dec_to_sql, ts_from_sql, ts_to_sql};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EquitySample {
    pub session_id: String,
    pub sampled_at: DateTime<Utc>,
    pub settlement_asset: String,
    pub equity: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
}

pub fn insert_equity_sample(conn: &Connection, sample: &EquitySample) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO equity_samples (
            session_id, sampled_at_ms, settlement_asset, equity, realized_pnl, unrealized_pnl
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(session_id, settlement_asset, sampled_at_ms) DO UPDATE SET
            equity = excluded.equity,
            realized_pnl = excluded.realized_pnl,
            unrealized_pnl = excluded.unrealized_pnl",
        params![
            sample.session_id,
            ts_to_sql(sample.sampled_at),
            sample.settlement_asset,
            dec_to_sql(sample.equity),
            dec_to_sql(sample.realized_pnl),
            dec_to_sql(sample.unrealized_pnl),
        ],
    )?;
    Ok(())
}

/// 最近的样本先在 SQL 中限量，再恢复成时间正序供曲线与日收益使用。
pub fn recent_equity_samples(
    conn: &Connection,
    session_id: &str,
    asset: &str,
    limit: usize,
) -> Result<Vec<EquitySample>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT sampled_at_ms, equity, realized_pnl, unrealized_pnl
         FROM equity_samples
         WHERE session_id = ?1 AND settlement_asset = ?2
         ORDER BY sampled_at_ms DESC LIMIT ?3",
    )?;
    let mut rows = stmt.query(params![session_id, asset, limit as i64])?;
    let mut samples = Vec::new();
    while let Some(row) = rows.next()? {
        let ms: i64 = row.get(0)?;
        let equity: String = row.get(1)?;
        let realized: String = row.get(2)?;
        let unrealized: String = row.get(3)?;
        samples.push(EquitySample {
            session_id: session_id.to_owned(),
            sampled_at: ts_from_sql(ms)?,
            settlement_asset: asset.to_owned(),
            equity: dec_from_sql(&equity)?,
            realized_pnl: dec_from_sql(&realized)?,
            unrealized_pnl: dec_from_sql(&unrealized)?,
        });
    }
    samples.reverse();
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn samples_are_exact_and_separated_by_session_and_asset() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::configure(&conn).unwrap();
        crate::schema::migrate(&conn).unwrap();
        let at = DateTime::from_timestamp_millis(1_780_000_000_000).unwrap();
        for (session, asset, equity) in [
            ("run-a", "USDC", dec!(10000.00000001)),
            ("run-a", "USDT", dec!(9)),
            ("run-b", "USDC", dec!(20)),
        ] {
            insert_equity_sample(
                &conn,
                &EquitySample {
                    session_id: session.into(),
                    sampled_at: at,
                    settlement_asset: asset.into(),
                    equity,
                    realized_pnl: dec!(0.00000001),
                    unrealized_pnl: Decimal::ZERO,
                },
            )
            .unwrap();
        }
        let rows = recent_equity_samples(&conn, "run-a", "USDC", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].equity, dec!(10000.00000001));
        assert_eq!(rows[0].realized_pnl, dec!(0.00000001));
    }
}
