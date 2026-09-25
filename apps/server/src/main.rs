//! 服务端入口。
//!
//! # 启动顺序
//!
//! 1. 解析配置（环境变量）
//! 2. 校验绑定地址必须是回环
//! 3. 打开 SQLite 并执行迁移（版本不符则拒绝启动）
//! 4. 完整性检查（孤儿成交、未知状态订单）
//! 5. 构造引擎与 API 状态
//! 6. 启动 HTTP 服务
//!
//! **启动时不做网络请求**——不拉 exchangeInfo 也不连 WebSocket。理由：网络
//! 不可用不应该阻止服务启动（否则离线时连界面都打不开），而合约规则与行情
//! 连接是后续的独立步骤，界面会显示它们的就绪状态。
//!
//! # 安全边界
//!
//! - 只绑定回环地址，且**无法通过配置绕过**。API 没有登录认证，暴露到公网
//!   等于把下单能力交给同网络上的任何人。
//! - API Key 从环境变量读取，不落盘、不打印。

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use domain::{ContractKind, FeeSchedule, FeeSource, Instrument, Precision, ServiceMode};
use engine::EngineConfig;
use rust_decimal::Decimal;

/// 默认数据根目录。
const DEFAULT_DATA_ROOT: &str = "data";
/// 默认绑定地址。**保持回环**——API 无认证。
const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// 默认交易对。
const DEFAULT_SYMBOL: &str = "ETHUSDC";

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;

    let symbol = std::env::var("RUST_CRYPTO_SYMBOL").unwrap_or_else(|_| DEFAULT_SYMBOL.to_string());
    let data_root = PathBuf::from(
        std::env::var("RUST_CRYPTO_DATA_ROOT").unwrap_or_else(|_| DEFAULT_DATA_ROOT.to_string()),
    );
    let bind = std::env::var("RUST_CRYPTO_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());

    // 只允许回环地址。
    //
    // API 没有认证。绑定到 0.0.0.0 等于把下单能力交给同网络上的任何人。
    // 这不是可通过配置绕过的限制——真需要远程访问时应走 SSH 隧道。
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("绑定地址非法：{bind}"))?;
    if !addr.ip().is_loopback() {
        bail!(
            "拒绝绑定到非回环地址 {addr}。API 没有登录认证，暴露到公网等于把\
             下单能力交给任何人。如需远程访问请用 SSH 隧道。"
        );
    }

    // 模式默认模拟盘，且只接受精确匹配。
    let mode = match std::env::var("RUST_CRYPTO_MODE") {
        Ok(s) => ServiceMode::parse(&s).map_err(|e| anyhow::anyhow!(e.to_string()))?,
        Err(_) => ServiceMode::Paper,
    };

    std::fs::create_dir_all(&data_root)
        .with_context(|| format!("创建数据目录失败：{}", data_root.display()))?;

    println!("rust-crypto 服务");
    println!("  数据根目录  {}", data_root.display());
    println!("  交易对      {symbol}");
    println!(
        "  模式        {}（{}）",
        mode.label(),
        if mode.is_live() { "LIVE" } else { "PAPER" }
    );
    println!("  监听        http://{addr}");
    println!();

    // ---- 打开数据库并迁移 ----
    let db_path = data_root.join("state").join("hot.sqlite3");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("打开数据库失败：{}", db_path.display()))?;
    store::configure(&conn)?;
    // 版本不符会在这里报错并退出，而不是按旧结构读写损坏数据。
    store::migrate(&conn).with_context(|| "数据库迁移失败")?;

    // ---- 完整性检查 ----
    let report = store::integrity_check(&conn).context("数据库完整性检查失败")?;
    println!(
        "数据库 {}（schema v{}）",
        db_path.display(),
        report.schema_version
    );
    if report.orphan_fills > 0 {
        // 孤儿成交意味着数据损坏。必须在启动时就暴露，而不是等到对账时
        // 才发现持仓对不上。
        bail!(
            "数据库有 {} 条成交找不到对应订单，数据可能已损坏。拒绝启动。",
            report.orphan_fills
        );
    }
    if report.unknown_state_orders > 0 {
        println!(
            "  注意：有 {} 张订单处于未知状态，需要查询对账后才能恢复交易。",
            report.unknown_state_orders
        );
    }
    if report.positions > 0 {
        println!("  注意：数据库中有 {} 个持仓。", report.positions);
    }
    println!();

    // ---- 构造引擎 ----
    let config = EngineConfig {
        instrument: instrument_for(&symbol),
        limits: domain::RiskLimits::default(),
        initial_equity: Decimal::from(10_000),
        assumed_latency_ms: 100,
        max_candles: 120,
        max_staleness_secs: 15,
    };

    let state = api::AppState::new(engine::PaperEngine::new(config), conn, data_root.clone());
    state.set_mode(mode).await;

    // ---- 路由 ----
    let app = api::router(state.clone()).layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("绑定 {addr} 失败（端口可能已被占用）"))?;

    println!("已启动。健康检查：http://{addr}/api/v1/health");
    println!("按 Ctrl-C 停止。");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP 服务异常退出")?;

    println!("\n已停止。");
    Ok(())
}

fn init_tracing() -> Result<()> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("server=info,api=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .map_err(|e| anyhow::anyhow!("初始化日志失败：{e}"))
}

/// 优雅停机信号。
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("注册 Ctrl-C 处理器失败");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("注册 SIGTERM 处理器失败")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    println!("\n收到停止信号，正在关闭...");
}

/// 构造合约规则。
///
/// **当前是硬编码的实测值**（2026-09 从 `exchangeInfo` 读取）。接入
/// `exchange::BinanceClient` 后应改为启动时拉取。在那之前，这些值会随币安
/// 调整而失效——所以界面会显示费率来源为「活动假设」，提醒结果不可直接
/// 用于决策。
fn instrument_for(symbol: &str) -> Instrument {
    let is_usdc = symbol.ends_with("USDC");
    let (kind, quote, base) = if is_usdc {
        (
            ContractKind::CryptoPerp,
            "USDC",
            symbol.trim_end_matches("USDC"),
        )
    } else {
        (
            ContractKind::TradFiPerp,
            "USDT",
            symbol.trim_end_matches("USDT"),
        )
    };

    Instrument {
        symbol: symbol.to_string(),
        kind,
        base_asset: base.to_string(),
        quote_asset: quote.to_string(),
        margin_asset: quote.to_string(),
        settlement_asset: quote.to_string(),
        precision: Precision {
            tick_size: Decimal::new(1, 2),
            step_size: Decimal::new(1, 3),
            min_qty: Decimal::new(1, 3),
            min_notional: Decimal::from(5),
        },
        // 实测值 2.5%，不是旧实现硬编码的 0.4%。
        maint_margin_pct: Decimal::new(25, 1),
        required_margin_pct: Decimal::from(5),
        liquidation_fee: Decimal::new(125, 4),
        fees: FeeSchedule {
            maker_rate: Decimal::ZERO,
            taker_rate: Decimal::new(5, 4),
            // 活动费率尚未与账户对账，所以标记为假设。
            // 回测结果会因此被标记为不完整。
            source: FeeSource::PromotionalAssumed,
            observed_at: chrono::Utc::now(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usdc_symbols_get_usdc_settlement() {
        let i = instrument_for("ETHUSDC");
        assert_eq!(i.settlement_asset, "USDC");
        assert_eq!(i.margin_asset, "USDC");
        assert_eq!(i.base_asset, "ETH");
        assert_eq!(i.kind, ContractKind::CryptoPerp);
    }

    /// TradFi 合约全部以 USDT 结算——与 USDC 是两套资产体系。
    #[test]
    fn tradfi_symbols_get_usdt_settlement() {
        let i = instrument_for("XAUUSDT");
        assert_eq!(i.settlement_asset, "USDT");
        assert_eq!(i.base_asset, "XAU");
        assert_eq!(i.kind, ContractKind::TradFiPerp);
    }

    /// 维持保证金率必须是真实的 2.5%——旧实现硬编码 0.4% 会让高杠杆下
    /// 误判止损不安全并静默拒绝信号。
    #[test]
    fn maintenance_margin_is_the_real_value() {
        assert_eq!(
            instrument_for("ETHUSDC").maint_margin_pct,
            Decimal::new(25, 1)
        );
    }

    /// 费率来源必须标记为「活动假设」，这样回测才会被标记为不完整。
    #[test]
    fn fee_source_is_marked_as_assumed() {
        let i = instrument_for("ETHUSDC");
        assert_eq!(i.fees.maker_rate, Decimal::ZERO);
        assert_eq!(i.fees.source, FeeSource::PromotionalAssumed);
        assert!(!i.fees.source.is_authoritative());
    }

    /// 默认绑定必须是回环——API 没有认证。
    #[test]
    fn default_bind_is_loopback() {
        let addr: SocketAddr = DEFAULT_BIND.parse().unwrap();
        assert!(
            addr.ip().is_loopback(),
            "默认绑定必须回环，否则下单能力会暴露给同网络"
        );
    }

    /// 默认值与模式必须是最安全的选择。
    #[test]
    fn defaults_are_safe() {
        assert_eq!(DEFAULT_SYMBOL, "ETHUSDC");
        assert_eq!(ServiceMode::default(), ServiceMode::Paper);
        assert!(!ServiceMode::default().allows_real_orders());
    }
}
