//! 数据库 schema 与迁移。
//!
//! # 迁移必须是真的迁移
//!
//! 旧实现用 JSON 文件 + `schema_version` 字段，但"迁移"只是 serde 的
//! `#[serde(default)]`——缺失字段被静默填成当前格式的默认值。那等价于
//! "格式变了也当没变"，一旦结构真的漂移就会静默错解数据。
//!
//! 这里用 SQLite 的 `PRAGMA user_version` 存版本号，每个版本对应一段**显式的**
//! DDL 迁移。升级路径是逐版本累加的，不可跳过；未知的更高版本直接拒绝启动，
//! 因为那意味着用户用了更新的程序读写过，降级运行会损坏数据。
//!
//! # 表设计要点
//!
//! - 所有金额、价格、数量都是 `TEXT`，存 `Decimal` 的字符串形式。
//!   不用 `REAL`（浮点误差），也不用 `INTEGER`（SQLite 整数无法表达
//!   `Decimal` 的完整精度范围，且定点缩放会把精度约定泄漏到 SQL 层）。
//! - 所有时间都是 `INTEGER` 毫秒 UTC。SQLite 没有原生时间类型。
//! - 结算资产是显式列，不合并 USDT 与 USDC。
//! - `client_order_id` 是本系统自己的唯一标识，是主键。post-only 被拒时
//!   交易所不保留任何记录，所以只能靠它。

use rusqlite::Connection;

/// 当前 schema 版本。新增迁移时递增。
pub const CURRENT_VERSION: i32 = 1;

/// 逐版本迁移。索引 `i` 对应"从版本 i 升到 i+1"。
///
/// 新增版本时必须**追加**，不能修改已有条目——已经跑过的迁移在用户机器上
/// 不会重跑，改动它只会让新旧库产生结构差异。
const MIGRATIONS: &[&str] = &[
    // v0 -> v1：初始结构
    r#"
    -- 订单。一张订单的完整生命周期记录。
    CREATE TABLE orders (
        client_order_id  TEXT PRIMARY KEY,
        exchange_order_id TEXT,
        symbol           TEXT NOT NULL,
        purpose          TEXT NOT NULL,   -- ENTRY / TAKE_PROFIT / STOP_LOSS
        side             TEXT NOT NULL,   -- BUY / SELL
        quantity         TEXT NOT NULL,   -- Decimal 字符串
        limit_price      TEXT NOT NULL,   -- Decimal 字符串
        tif              TEXT NOT NULL,   -- POST_ONLY / POST_ONLY_GTD
        gtd_deadline_ms  INTEGER,
        reduce_only      INTEGER NOT NULL DEFAULT 0,
        parent_id        TEXT REFERENCES orders(client_order_id),
        state            TEXT NOT NULL,   -- OrderState 的序列化形式
        filled_quantity  TEXT NOT NULL DEFAULT '0',
        avg_price        TEXT,
        reject_reason    TEXT,
        created_at_ms    INTEGER NOT NULL,
        updated_at_ms    INTEGER NOT NULL
    );
    CREATE INDEX idx_orders_symbol_state ON orders (symbol, state);
    CREATE INDEX idx_orders_updated ON orders (updated_at_ms DESC);

    -- 成交明细。按交易对与时间查询（交易总览）。
    CREATE TABLE fills (
        trade_id         TEXT PRIMARY KEY,
        client_order_id  TEXT NOT NULL REFERENCES orders(client_order_id),
        symbol           TEXT NOT NULL,
        quantity         TEXT NOT NULL,
        price            TEXT NOT NULL,
        fee              TEXT NOT NULL,
        fee_asset        TEXT NOT NULL,
        filled_at_ms     INTEGER NOT NULL
    );
    CREATE INDEX idx_fills_symbol_time ON fills (symbol, filled_at_ms DESC);
    CREATE INDEX idx_fills_order ON fills (client_order_id);

    -- 持仓快照。持仓是状态而非历史，所以只保留当前值，
    -- 历史由 fills 与 orders 重建。
    CREATE TABLE positions (
        symbol           TEXT PRIMARY KEY,
        side             TEXT NOT NULL,
        quantity         TEXT NOT NULL,
        entry_price      TEXT NOT NULL,
        stop_price       TEXT,
        opened_at_ms     INTEGER NOT NULL,
        updated_at_ms    INTEGER NOT NULL
    );

    -- 已实现盈亏，按结算资产分开记账。绝不能把 USDT 与 USDC 相加。
    CREATE TABLE realized_pnl (
        settlement_asset TEXT PRIMARY KEY,
        amount           TEXT NOT NULL
    );

    -- 回测运行记录。结果文件在 data/backtests/，这里只存索引与元信息。
    CREATE TABLE backtest_runs (
        run_id           TEXT PRIMARY KEY,
        symbol           TEXT NOT NULL,
        strategy_id      TEXT NOT NULL,
        strategy_params  TEXT NOT NULL,   -- JSON
        fill_model       TEXT NOT NULL,
        initial_equity   TEXT NOT NULL,
        final_equity     TEXT NOT NULL,
        trade_count      INTEGER NOT NULL,
        start_ms         INTEGER NOT NULL,
        end_ms           INTEGER NOT NULL,
        -- 结论可信度。这几个字段不是装饰，是判断结果能否用于决策的依据。
        sign_flips       INTEGER NOT NULL DEFAULT 0,
        breakeven_fill_rate TEXT,
        adverse_markout_5s  TEXT,
        fee_incomplete   INTEGER NOT NULL DEFAULT 0,
        gap_count        INTEGER NOT NULL DEFAULT 0,
        created_at_ms    INTEGER NOT NULL
    );
    CREATE INDEX idx_runs_symbol_time ON backtest_runs (symbol, created_at_ms DESC);

    -- AI 会话历史。保存提问、回答与结构化标注，供复盘。
    CREATE TABLE ai_sessions (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        symbol        TEXT NOT NULL,
        interval      TEXT NOT NULL,
        question      TEXT NOT NULL,
        answer        TEXT NOT NULL,
        -- 模型产出的结构化标注（Drawing JSON 数组）。与用户手画的线
        -- 是同一套类型，所以能直接渲染到图上。
        annotations   TEXT NOT NULL DEFAULT '[]',
        source        TEXT NOT NULL,     -- 例如 deepseek / local-deterministic
        model         TEXT,
        context_json  TEXT NOT NULL DEFAULT '{}',
        created_at_ms INTEGER NOT NULL
    );
    CREATE INDEX idx_ai_symbol_time ON ai_sessions (symbol, created_at_ms DESC);

    -- 用户手画的标注。与 AI 标注同表不同 source 会混淆展示，所以分开。
    CREATE TABLE annotations (
        id            TEXT PRIMARY KEY,
        symbol        TEXT NOT NULL,
        interval      TEXT NOT NULL,
        kind          TEXT NOT NULL,
        source        TEXT NOT NULL,     -- user / ai / strategy
        payload_json  TEXT NOT NULL,
        label         TEXT,
        visible       INTEGER NOT NULL DEFAULT 1,
        locked        INTEGER NOT NULL DEFAULT 0,
        created_at_ms INTEGER NOT NULL
    );
    CREATE INDEX idx_annotations_symbol ON annotations (symbol, interval);
    "#,
];

/// 当前数据库版本。
pub fn version(conn: &Connection) -> rusqlite::Result<i32> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// 把数据库迁移到 `CURRENT_VERSION`。
///
/// 逐版本累加执行，不可跳过。数据库版本高于程序时**报错而非继续**——
/// 那说明用过更新的程序，按旧结构读写会损坏数据。
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut current = version(conn)?;

    if current > CURRENT_VERSION {
        // 用 rusqlite 的错误类型承载，但信息必须清晰。
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "数据库版本 {current} 高于本程序支持的 {CURRENT_VERSION}，\
                 拒绝按旧结构读写以免损坏数据。请升级程序或从备份恢复。"
            )),
        ));
    }

    while current < CURRENT_VERSION {
        let sql = MIGRATIONS[current as usize];
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        // user_version 不能用参数绑定，只能拼接——这里拼接的是编译期常量，
        // 不涉及用户输入，无注入风险。
        tx.execute_batch(&format!("PRAGMA user_version = {}", current + 1))?;
        tx.commit()?;
        current += 1;
        tracing::info!(version = current, "数据库迁移完成");
    }

    Ok(())
}

/// 打开连接并设定运行时参数。
///
/// - WAL 模式：读写不互相阻塞，更适合"写入订单的同时查询状态"。
/// - `busy_timeout`：WAL 下仍有短暂写锁竞争，超时后重试而不是立即报错。
/// - `foreign_keys`：SQLite 默认关闭外键约束，必须显式开启，
///   否则 `fills.client_order_id` 引用的订单可以被删掉而留下孤儿记录。
/// - `synchronous = FULL`：订单与成交必须落盘。这是交易系统，
///   不能用 NORMAL 换性能——崩溃丢掉刚成交的记录会导致持仓对账错误。
pub fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        conn
    }

    #[test]
    fn fresh_database_migrates_to_current_version() {
        let conn = fresh();
        assert_eq!(version(&conn).unwrap(), 0);
        migrate(&conn).unwrap();
        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = fresh();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
    }

    /// 数据库版本高于程序时必须拒绝，否则会按旧结构读写新数据。
    #[test]
    fn newer_database_version_is_rejected() {
        let conn = fresh();
        conn.execute_batch(&format!("PRAGMA user_version = {}", CURRENT_VERSION + 5))
            .unwrap();
        let err = migrate(&conn).unwrap_err().to_string();
        assert!(err.contains("高于"), "{err}");
        assert!(err.contains("拒绝"), "{err}");
    }

    #[test]
    fn all_expected_tables_exist() {
        let conn = fresh();
        migrate(&conn).unwrap();
        for table in [
            "orders",
            "fills",
            "positions",
            "realized_pnl",
            "backtest_runs",
            "ai_sessions",
            "annotations",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "表 {table} 应存在");
        }
    }

    /// 外键约束必须真的生效，否则会留下孤儿成交记录。
    #[test]
    fn foreign_keys_are_enforced() {
        let conn = fresh();
        migrate(&conn).unwrap();
        let r = conn.execute(
            "INSERT INTO fills (trade_id, client_order_id, symbol, quantity, price, fee, fee_asset, filled_at_ms)
             VALUES ('t1', 'nonexistent-order', 'ETHUSDC', '1', '3200', '0', 'USDC', 0)",
            [],
        );
        assert!(r.is_err(), "引用不存在的订单必须被拒绝");
    }

    /// 金额列必须是 TEXT：用 REAL 会引入浮点误差，而止盈目标是 bp 级。
    #[test]
    fn monetary_columns_are_text_not_real() {
        let conn = fresh();
        migrate(&conn).unwrap();
        let mut stmt = conn
            .prepare("SELECT name, type FROM pragma_table_info('orders')")
            .unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();

        for name in ["quantity", "limit_price", "filled_quantity", "avg_price"] {
            let ty = cols
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            assert_eq!(
                ty.to_uppercase(),
                "TEXT",
                "{name} 必须是 TEXT 以精确保存 Decimal"
            );
        }
    }

    /// 结算资产必须是显式列——USDT 与 USDC 不能混。
    #[test]
    fn settlement_asset_is_explicitly_tracked() {
        let conn = fresh();
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('fills') WHERE name='fee_asset'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        let n2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('realized_pnl') WHERE name='settlement_asset'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n2, 1, "已实现盈亏必须按结算资产分开记账");
    }

    /// 结论可信度字段必须落库——否则复盘时无法判断某个结果能不能用。
    #[test]
    fn backtest_runs_persist_conclusion_confidence() {
        let conn = fresh();
        migrate(&conn).unwrap();
        for col in [
            "sign_flips",
            "breakeven_fill_rate",
            "adverse_markout_5s",
            "fee_incomplete",
            "gap_count",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('backtest_runs') WHERE name=?1",
                    [col],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "backtest_runs 应含 {col} 列");
        }
    }
}
