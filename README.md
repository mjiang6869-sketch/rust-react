# Rust Crypto 模拟做市台

独立的 Rust + React 项目。第一阶段复现原 Python 模拟盘的“15 分钟区间突破失败后回踩挂单”策略：用币安公开 1 分钟 K 线生成信号，本地模拟开仓、Maker 止盈与限价止损。**当前不连接交易账户，也不会提交真实订单。**

## 运行

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

当前撮合基于最新 K 线价格，尚无真实订单簿、部分成交或交易所排队位置模型。接入真实交易前，需要实现用户数据流、账户保证金与实际费率核验、超时订单对账、完整的订单簿回放及连续纸盘验证。API 目前只绑定本机回环地址且未加登录认证，不能直接对外开放。
