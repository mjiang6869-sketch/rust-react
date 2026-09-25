# Rust Crypto 自动交易研究桌面端

Rust + React + Electron 的币安 USDⓈ-M 永续合约交易研究平台。默认使用 PAPER 模式，支持失败突破回踩和缠论中枢回踩策略、K 线/盘口回测、趋势与笔线段中枢标注、只读 AI 会话分析，以及经过账户对账和显式 ARM 的 LIVE Maker-only 执行。

LIVE 真实订单只允许 `LIMIT + GTX` Maker-only，止盈和止损使用 reduce-only 限价保护单；任何超时、断线、未知订单状态或远端持仓不一致都会进入查询、只减仓或 DISARM。不会使用市价单兜底，也不会从 PAPER 自动切换 LIVE。

后续 AI 修改本项目时必须遵守根目录的 [`AGENTS.md`](./AGENTS.md)，其中记录了项目目的、USDT/USDC/TradFi 资产语义、交易安全边界、代码规范、测试和 Git 提交要求。

## 一键运行

需要 Rust 稳定版、Node.js 20+、pnpm 和 curl。首次运行会按需安装前端/Electron 依赖并编译 Rust sidecar：

```sh
./scripts/run.sh              # 启动 Electron、Rust API 和 Vite
./scripts/run.sh restart      # 重启全部本地进程
./scripts/run.sh status       # 查看进程和 API 健康状态
./scripts/run.sh logs         # 查看 Electron/Rust/Vite 日志
./scripts/run.sh stop         # 停止本脚本启动的进程
```

运行目录是 `.runtime/`，不会写入 Git；默认状态文件仍按交易对保存到 `data/`。脚本只停止自己记录的 Electron 进程及其子进程，不会杀掉其他项目或端口上的进程。

## 手动运行

需要 Rust 稳定版、Node.js 20+ 和 pnpm。两个终端分别运行：

```sh
cd /Users/mingjiangliu/dev/cryptocurrency/rust-crypto
cargo run
```

```sh
cd /Users/mingjiangliu/dev/cryptocurrency/rust-crypto/frontend
pnpm install
pnpm dev
```

打开 `http://127.0.0.1:5174`。Rust API 默认监听 `127.0.0.1:8080`。启动时需要能访问币安 USDⓈ-M 公开 `exchangeInfo`、历史 K 线和 WebSocket；不能核验合约规则时会拒绝启动。

默认交易对为 `ETHUSDC`。单次只运行一个交易对，切换时先停掉进程，再设置 `RUST_CRYPTO_SYMBOL`，例如：

```sh
RUST_CRYPTO_SYMBOL=ETHUSDT cargo run
RUST_CRYPTO_SYMBOL=XAUUSDT cargo run
```

每个交易对使用独立的 `data/{symbol}.json` 状态文件。`ETHUSDC` 和 `ETHUSDT` 分开保存；TradFi 合约通过币安的 `TRADIFI_PERPETUAL` 类型识别。启动时刷新价格精度、数量精度和最小名义价值；配置、订单、持仓和已使用信号写入原子替换的状态文件，同一文件只允许一个进程写入。

## 资金和手续费

- 模拟钱包分别记录 USDT 与 USDC。默认有 10,000 USDT、0 USDC，策略默认关闭。
- 多资产模式模拟全仓共享保证金：USDT 可以为 USDC 本位合约提供可用额；合约盈亏和 Maker 手续费仍记入合约的结算资产。关闭多资产模式后，只使用结算资产余额。
- 模拟盘暂按 USDT 与 USDC 1:1 估值，未实现真实账户的抵押品折算、自动兑换、资金费、爆仓撮合或盘口排队。不能把“可用保证金估值”当作真实账户可用余额。
- Maker 费率是配置项。新建 USDT 加密合约模拟账户的初值为 0.02%，USDC 与 TradFi 初值为 0%；这些只是模拟初值，需按账户实际费率调整。优惠活动变更后不会自动同步。
- 限价止损触发后仍须等待限价成交；跳价可能留下未平仓风险。停用策略会撤销开仓单，已有模拟保护单继续处理。

## 检查

```sh
cd /Users/mingjiangliu/dev/cryptocurrency/rust-crypto
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

```sh
cd /Users/mingjiangliu/dev/cryptocurrency/rust-crypto/frontend
pnpm build
```

## 后续阶段

## Cloudflare 配置

Cloudflare Worker 配置位于 `cloudflare/worker`，绑定当前账户的 D1、R2 和下载队列。首次部署前在 Node.js 22+ 环境执行：

```sh
cd /Users/mingjiangliu/dev/cryptocurrency/rust-crypto/cloudflare/worker
pnpm install
pnpm typecheck
pnpm exec wrangler deploy
```

D1 资源、R2 绑定和队列生产者已写入 `cloudflare/worker/wrangler.toml`。本地可运行 `./scripts/run.sh cloudflare-check` 做 Worker 类型检查；远端迁移和部署是显式操作：

```sh
wrangler secret put RUST_CRYPTO_INTERNAL_TOKEN --config cloudflare/worker/wrangler.toml
./scripts/run.sh cloudflare-migrate
./scripts/run.sh cloudflare-deploy
```

Worker 会把请求头与 `RUST_CRYPTO_INTERNAL_TOKEN` Secret 比对，不能只设置请求头。`cloudflare/d1/002_operational_tables.sql` 补齐了数据缺口和回测运行表。当前 Cloudflare 部分仍是元数据/任务入口：队列消费者、R2 数据下载归档和远端回测 worker 尚未在本仓库实现，因此不能宣称云端历史数据和异步回测链路已全部上线。

当前数据契约同时覆盖两类产品：USDC 本位加密永续（例如 `ETHUSDC`）和 TradFi 美股合约（`contract_type = TRADIFI_PERPETUAL`）。两者在 `instruments` 中分别保存 `quote_asset`、`margin_asset`、`settlement_asset`、交易时段与 Maker/Taker 费率；Worker 可通过 `GET /internal/instruments?product_type=crypto_usdc_perpetual` 或 `GET /internal/instruments?product_type=tradfi_equity_perpetual` 查询。USDT 余额对 USDC 合约的共享保证金仍只属于显式多资产模式，不能把两种结算资产写成一个余额。

PAPER 撮合支持 K 线触价和可选的最优盘口部分成交模型；它仍不代表真实队列位置、资金费、抵押品折算或强平撮合。LIVE 适配器已实现用户数据流、账户可用余额核验、订单查询恢复和远端持仓对账，但必须使用允许的 endpoint、独立凭据和显式 ARM。API 只绑定本机回环地址且未加登录认证，不能直接对外开放。
