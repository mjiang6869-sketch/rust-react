# Rust Crypto — 币安合约做市研究平台

Rust + React 的币安 USDⓈ-M 永续合约做市研究平台。本地运行，默认模拟盘。

## 这个项目解决什么问题

除了回测与模拟盘，它要解决一件**币安原生做不到**的事：**一个点位挂单、同时挂
分批止盈与止损**。

币安 USDⓈ-M 没有 OCO，也没有 bracket 单。支持的条件单只有：

```text
STOP / STOP_MARKET / TAKE_PROFIT / TAKE_PROFIT_MARKET / TRAILING_STOP_MARKET
```

一张单只能对应一个数量与一个触发价。所以下面四件事必须由客户端实现：

1. 一个点位挂单，同时挂分批止盈与止损
2. 分批止盈（例如 40%/30%/30%），三张单分开挂、分别成交
3. 保本止损：浮盈后把止损推到入场价，需要撤单重挂
4. 止损成交后撤销未成交的止盈单（没有 OCO 自动做这件事）

## 快速开始

需要 Rust 稳定版、Node.js 20+、pnpm、curl。

```sh
git clone <repo> && cd rust-crypto
./scripts/run.sh
```

首次运行会自动安装前端依赖并编译 Rust 服务（较慢），之后几秒内启动。

打开 `http://127.0.0.1:5174`。

### 脚本命令

```sh
./scripts/run.sh              # 启动（默认）
./scripts/run.sh restart      # 重启
./scripts/run.sh stop         # 停止
./scripts/run.sh status       # 查看进程与健康状态
./scripts/run.sh logs         # 跟踪日志（Ctrl-C 退出，不停服务）
./scripts/run.sh build        # 只构建，不启动
```

脚本会自己发现 `cargo` 与 `pnpm` 的位置——从 GUI/IDE 启动的 shell 通常不
继承登录 shell 的 PATH，不需要手动 `export`。

### 环境变量

```sh
RUST_CRYPTO_SYMBOL=ETHUSDC         # 交易对
RUST_CRYPTO_MODE=PAPER             # PAPER（默认）或 LIVE
RUST_CRYPTO_DATA_ROOT=./data       # 数据目录
RUST_CRYPTO_BIND=127.0.0.1:8080    # 后端监听地址
RUST_CRYPTO_FRONTEND_PORT=5174     # 前端端口
RC_RELEASE=1                       # 用 release 构建（运行更快）
```

运行目录是 `.runtime/`（日志与 PID），不写入 Git。

## 当前状态

**默认模拟盘。** 真实下单能力需要显式 `RUST_CRYPTO_MODE=LIVE` 且通过三个
前置条件（用户数据流连接、账户对账、显式 ARM）。

| 能力 | 状态 |
| --- | --- |
| 历史数据下载与转换 | ✅ |
| 回测（含反自欺指标） | ✅ |
| 手动下单（分批止盈 + 止损 + 保本） | ✅ |
| 数据管理与覆盖查询 | ✅ |
| 实时行情接入 | ❌ 未完成——界面图表尚无数据 |
| 实盘下单 | ⚠️ 客户端已实现，尚未接入事件循环 |

**实时行情还没有接进来**：引擎、撮合、WebSocket 都是通的，但没有行情源喂
数据。所以图表是空的，模拟盘也不会成交。

## 架构

Workspace 结构，依赖方向严格向内：

```
crates/
├── domain/       零 I/O 领域层
│                 量化 · 合约规则 · 行情事件 · 策略接口 · 风控
│                 订单状态机 · 保护单规划器 · 持仓-保护单一致性 · 运行模式
├── data/         历史数据
│                 下载器 · 台账 · 定点编码 · Parquet 转换 · 回放
├── sim/          撮合与回测
│                 成交模型 M0/M1 · 成交带 · 反自欺指标 · 回测引擎
├── store/        热状态持久化（SQLite）
├── strategies/   策略实现
├── exchange/     币安客户端（签名 · 合约规则 · REST）
├── engine/       编排层（模拟盘引擎）
├── api/          HTTP + WebSocket
└── cli/          命令行（download / backtest / coverage）
apps/server/      服务端入口
frontend/         React 界面
```

**`domain` 的零 I/O 是硬约束**，CI 校验：

```sh
cargo tree -p domain | grep -E 'tokio|reqwest|duckdb|axum'   # 必须无输出
```

这条约束换来的是：回测、模拟盘、实盘共用同一套订单生命周期与保护单数学，
全部可用纯函数测试覆盖。

### 为什么回测的成交模型是核心

零手续费做市下，单笔毛利润就是止盈距离（约 4bp）。没有价差缓冲、没有返佣、
没有逆向选择垫。**成交模型乐观 10% 不是让 P&L 差 10%，而是可能翻转符号**：
亏损的交易（突破把你扫掉）建模得近乎完美，盈利的交易（需要对手方主动吃你
的挂单）被建模成免费。

所以回测必须同时跑两个模型并报告差异：

| 模型 | 含义 |
| --- | --- |
| `M0` | 上界：wick 触价即全额成交。**不现实，仅作对照** |
| `M1` | 保守下界：要求真实成交发生在我们的价位，且由正确方向的主动单驱动 |

回测报告给出四个反自欺指标：盈亏平衡成交率、markout（逆向选择检测）、
止损裸露分析、费率贡献。

**排队位置无法从币安任何免费归档还原**——`aggTrades` 与 `trades` 都只有
`is_buyer_maker` 一个方向标志，没有订单 ID；`bookTicker` 已于 2024-04 停更。
所以 M1 的「排在队尾」是唯一可能的选择，不是保守近似。

## 检查

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo tree -p domain | grep -E 'tokio|reqwest|duckdb|axum'   # 必须无输出

cd frontend
pnpm typecheck
pnpm build
```

## 命令行

```sh
cargo run -p cli -- download --symbol ETHUSDC --kind klines,agg_trades \
    --from 2026-01 --to 2026-08
cargo run -p cli -- coverage --symbol ETHUSDC
cargo run -p cli -- backtest --symbol ETHUSDC \
    --from 2026-08-01 --to 2026-08-07 --fill-models m0,m1
cargo run -p cli -- strategies
cargo run -p cli -- models
```

## 数据

历史数据从 `data.binance.vision` 归档下载（CDN 静态 ZIP，支持 Range 断点续传）。

| 数据集 | 覆盖 |
| --- | --- |
| `klines/1m` | 2024-01 起 |
| `aggTrades` | 2024-01 起，成交模型与 markout 的数据源 |
| `markPriceKlines/1m` | 2024-01 起，强平距离 |
| `fundingRate` | 完整，持仓成本 |
| `trades` | **不使用**：字段与 aggTrades 同构，体积 2.5 倍 |
| `bookTicker` | **已停更**，不可用 |

数据落盘为 Parquet（Hive 分区），DuckDB 直接 SQL 查询。下载台账记录每个
分区的状态与校验和，重跑会跳过已完成的分区。

## 安全

- API 只绑定回环地址，**无法通过配置绕过**。它没有登录认证。
- API Key 从环境变量读取，不落盘、不打印。`Credentials` 的 `Debug` 是手工
  实现的，只显示「已隐藏」。
- 出场单**全部**是 reduce-only 的 GTX 限价单，不用市价单兜底。
- 网络超时与 5xx 归类为「状态未知」，必须先查询对账，绝不直接重试提交。
- 实盘要求用户数据流连接、账户对账、显式 ARM 三个条件同时成立；断线或不
  一致会**自动解除武装**；重启后不恢复武装状态。

## 约定

给 AI 编码代理的项目约定见 [`AGENTS.md`](./AGENTS.md)，包含交易安全不变量、
代码规范与测试要求。
