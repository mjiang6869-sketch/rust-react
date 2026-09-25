# 做市策略逻辑与 Rust 迁移契约

## 1. 文档目的

本文把旧 Python 项目中当前使用的 `market_making_retest` 做市回测和 Maker 执行逻辑，整理成 Rust + React 项目的可执行契约。它描述策略行为、回测假设、账户资产边界和迁移验收标准，供后续实现、评审和测试使用。

本文不包含真实下单实现，也不把研究脚本中的其他策略（Donchian、Fibonacci、Martingale 等）混入当前策略。当前 Rust 项目仍然是模拟盘。

## 2. 来源与迁移边界

旧项目的核心来源是：

- `backend/app/market_making/signals.py`：失败突破、回踩价格和信号失效规则。
- `backend/app/backtest/market_making_retest.py`：1 分钟 K 线回测、挂单、止盈、止损、强平和权益曲线。
- `backend/app/backtest/market_making_retest_adapter.py`：回测结果映射为通用交易明细。
- `backend/app/market_making/engine.py`：实时信号轮询和报价周期。
- `backend/app/live/maker_execution.py` 与 `backend/app/live/service.py`：Maker-only/GTX 订单、成交去重、撤单和状态跟踪。
- `backend/tests/test_market_making_retest_backtest.py`、`test_market_making_retest_orders.py`：边界行为和回归样例。

迁移原则：先保证固定输入下的行为一致，再替换为 Rust 的并发、持久化和云端数据管道。交易数值统一使用 `Decimal`，时间统一使用 UTC；不能照搬 Python 中的 `float` 或隐式资产合并。

## 3. 产品与资产模型

策略可以运行在以下三类产品上，但每个产品必须独立读取交易所规则：

| 产品 | `product_type` | `contract_type` | 结算与手续费要求 |
| --- | --- | --- | --- |
| USDT 本位加密永续 | `crypto_usdt_perpetual` | `PERPETUAL` | PnL、保证金和费用记入 USDT |
| USDC 本位加密永续 | `crypto_usdc_perpetual` | `PERPETUAL` | PnL、保证金和费用记入 USDC |
| TradFi 美股合约 | `tradfi_equity_perpetual` | `TRADIFI_PERPETUAL` | 按该合约的交易时段、结算资产和实际费率处理 |

每个 instrument 至少包含：`symbol`、`base_asset`、`quote_asset`、`margin_asset`、`settlement_asset`、`contract_type`、`tick_size`、`step_size`、`min_qty`、`min_notional`、最大杠杆、交易时段和 Maker/Taker 费率。

钱包必须有明确资产键：`wallet.usdt`、`wallet.usdc`。订单、成交、手续费、已实现 PnL、保证金和回测结果都必须携带 `settlement_asset`。

### USDT 与 USDC 的关系

币安多资产模式下，USDT 余额可能被允许作为 USDC 合约的共享保证金，但这不是“把 USDT 和 USDC 余额相加”。Rust 账户模型必须记录：

- `account_mode`：单资产或多资产；
- 抵押品折算率和资产可用额；
- 合约真正的 `margin_asset` 与 `settlement_asset`；
- 费用和 PnL 的记账资产。

模拟盘可以暂按 USDT/USDC 1:1 估值，但必须把它标记为模拟假设，不能直接当作真实账户可用余额。关闭多资产模式后，USDT 不能自动为 USDC 合约提供保证金。

## 4. 行情输入与时间规则

1. 权威输入是连续、已收盘的 1m K 线，字段至少包括开高低收、成交量、`open_time`、`close_time`、`is_closed` 和 UTC 时区。
2. 信号只能使用当前时刻之前已经闭合的 K 线；未闭合 K 线不能参与区间、突破或成交判断。
3. 实时行情必须有新鲜度检查。超过 15 秒没有更新时暂停开仓；断线恢复后先补 K 线、校验状态，再恢复策略。
4. 回测按 `open_time` 排序，过滤非 1m 或未收盘数据；`start_at` 前的数据只用于预热，不计入报告期交易。
5. 订单簿深度、成交明细和标记价格不是当前信号的必需输入，但真实交易和高保真回测阶段应保存并使用它们估计排队、部分成交、资金费和强平。

## 5. 信号生成

### 5.1 参考区间

在每个 15 分钟桶 `range_start` 内：

1. 取该桶开始前连续 60 根已收盘 1m K 线，即最近四根完整 15m 的分钟数据。
2. `range_low = min(low)`，`range_high = max(high)`。
3. 区间无效、数据缺失或价格精度未知时不生成信号。
4. 桶开始后的前 5 分钟暂停开仓，避免刚换区间时使用不完整结构。

### 5.2 失败突破

从新桶开始逐根读取已收盘 1m K 线：

- 先向上突破 `range_high`，记录上方极值，之后收盘回到 `(range_low, range_high)` 内，确认做空失败突破信号，`side = SELL`。
- 先向下跌破 `range_low`，记录下方极值，之后收盘回到区间内，确认做多失败突破信号，`side = BUY`。
- 同一根 K 线同时越过上下边界时，OHLC 无法判断先后，清除候选，不生成确定信号。
- 突破后再次朝突破方向创新极值但没有回到区间时，更新极值；价格回到区间才确认。

确认后冻结信号：

```text
SELL entry = 向上量化(range_high, tick_size)
SELL stop  = 向上量化(extreme, tick_size) + tick_size
BUY  entry = 向下量化(range_low, tick_size)
BUY  stop  = 向下量化(extreme, tick_size) - tick_size
```

信号默认只保留 2 分钟，且不能跨入下一个 15 分钟桶。止损距离必须通过风险过滤：入场价、止损价、止盈参数必须为正，且止损距离不能超过 `min(stop_pct, take_profit_pct * 3)`。

### 5.3 信号失效与去重

下列任一条件发生，撤销挂单并清除信号：

- 信号超过 2 分钟或进入新的 15 分钟桶；
- 当前 1m 行情缺失、过期或不在允许的滚动窗口；
- 做空信号再次触及/越过上方极值，或做多信号再次触及/越过下方极值；
- 做空价格到达区间低点，或做多价格到达区间高点。

同一个确认时间只能使用一次。停止策略、断线或重启时，旧的开仓信号不能静默复用。

## 6. 挂单与成交状态机

### 6.1 开仓挂单

信号确认后创建一个 Maker-only 限价开仓单：

1. 按 instrument 的 `tick_size`、`step_size`、`min_qty`、`min_notional` 量化和校验价格、数量。
2. 数量模式支持：
   - `fixed_quantity`：使用固定数量；
   - `equity_leverage`：`quantity = floor(equity * leverage / entry_price, step_size)`。
3. 检查数量非零、名义价值达标、杠杆和强平价可行；不满足时跳过开仓或结束本次回测。
4. 真实交易适配器使用 GTX/Maker-only 限价单，禁止用开仓市价单替代。
5. 默认挂单有效期 2 分钟；到期、换桶、行情过期或信号失效时撤单。

建议的 Rust 状态：

```text
SignalConfirmed -> EntryOpen -> EntryPartiallyFilled -> EntryFilled
                         |             |                 |
                         +-> Canceled  +-> Canceled      +-> PositionOpen
                         +-> Unknown   +-> Reconcile     +-> ExitPending
```

网络超时或撤单响应未知时，必须先查询远端订单状态，再决定是否重试；禁止直接重复提交。成交事件按 `fill_id` 或交易所成交 ID 去重。

### 6.2 持仓与退出

持仓状态必须包含方向、成交均价、数量、入场时间、止损价、结算资产、保证金、杠杆和远端订单 ID。保护单是 reduce-only，不能反向开仓。

- Maker 止盈：多头目标为 `entry * (1 + take_profit_pct)`，空头目标为 `entry * (1 - take_profit_pct)`，并按 tick 量化。
- 结构止损：使用信号冻结的 `stop_price`，触发后仍按限价保护单等待成交；不能默认用市价兜底掩盖未成交风险。
- 交易所支持时，止盈也使用 Maker-only；止损价格触发后进入 `StopTriggered`，限价未成交期间保持风险告警和只减仓状态。
- 同一根 K 线同时触及止盈和止损时，K 线无法证明先后。回测记录 `ambiguous`，保守权益采用止损，乐观权益采用止盈。
- 若强平价先于结构止损被触及，按破产价结算并停止继续开仓；不把权益算成负数。

退出后的状态必须先完成远端对账，再释放本地持仓和保证金。停用策略只撤销开仓单；已有持仓的保护逻辑仍需运行，直到平仓或进入人工处理。

## 7. 手续费、资金费与融资费

费率必须是 instrument/account 的配置快照，随每笔成交记录保存：`maker_fee_rate`、`taker_fee_rate`、`fee_asset`。

- USDT 加密合约的模拟初值可以为 0.02% Maker，但必须可配置。
- USDC 合约和 TradFi 的研究初值可以为 0%，但这只是初值，必须以账户或交易所当前规则核验，不能永久硬编码为零。
- 资金费、借贷/融资费、结算费和 TradFi 特有费用如果数据不可用，回测结果必须标记为不完整，不得悄悄当作零。
- 费用从成交的结算资产余额扣除，不能从 USDT/USDC 无标识的总余额扣除。

## 8. 回测撮合规则

回测必须在运行参数中记录撮合模型和策略版本：

1. 只读取当时已经收盘的 1m 数据，禁止未来函数。
2. 当前 Python 基线使用 K 线触价模型：多头开仓价被 `low` 触及即视为成交，空头开仓价被 `high` 触及即视为成交；退出同理。
3. 如果一根 K 线同时触发入场和止损，不能判定先后，入场订单记为 `AMBIGUOUS`，不生成确定持仓交易。
4. 如果持仓 K 线同时触及止盈和止损，记录 `ambiguous`，同时输出 conservative/optimistic 两条权益结果。
5. 挂单到期、换桶或信号失效时记为 `CANCELED`，统计 `pending_expiries`。
6. K 线跳空直接越过保护价时，默认按触价规则记录保护价成交，并在结果中标记模型限制；订单簿回放阶段改用实际可成交价格。
7. 交易数量、价格、名义价值和保证金先按交易规则量化。名义价值不足、数量归零或开仓即强平时，回测提前终止并记录原因。
8. 回测结果必须保存起止 UTC 时间、数据集 ID、数据缺口、费用快照、滑点模型、配置 hash、策略版本和结算资产。

## 9. Rust 模块映射

当前模块与迁移职责对应如下：

| Rust 模块 | 责任 |
| --- | --- |
| `src/feed.rs` | 读取 exchangeInfo、1m K 线、WebSocket/REST 兜底、行情新鲜度 |
| `src/signal.rs` | 15m 区间、失败突破、回踩入场、失效和风险过滤 |
| `src/paper.rs` | USDT/USDC 钱包、挂单、持仓、保护单、费用和模拟撮合 |
| `src/storage.rs` | 带 `schema_version` 的原子状态、单进程锁、重启恢复 |
| `src/main.rs` | API、任务编排和安全停机 |

后续建议拆出：

- `src/instrument.rs`：交易规则、产品类型和资产语义；
- `src/order_state.rs`：订单状态机、客户端 ID、成交去重和对账；
- `src/backtest.rs`：确定性回测撮合、歧义结果和权益曲线；
- `src/fees.rs`：费率快照、资金费和结算费；
- `src/cloud.rs`：R2 对象、D1 Worker 内部 API 和任务幂等。

React 只配置策略、展示行情/订单/持仓和回测结果，不能读取或提交 API Key，也不能绕过 Rust 的资产和风险校验。

## 10. 云端数据保存

历史行情和回测结果全部以云端为权威：

- R2 `rust-crypto-market-data`：原始压缩 K 线、标准化 Parquet、必要的成交/深度/资金费文件；
- R2 `rust-crypto-backtests`：回测摘要、交易明细、权益曲线和运行日志；
- D1 `rust-crypto-meta`：数据集索引、覆盖范围、checksum、缺口、策略版本、配置 hash 和任务状态；
- Queues：下载与回测任务，消息必须带唯一 `job_id`，消费者按至少一次投递设计为幂等。

文件上传并校验 checksum 后才能写 D1 manifest 为可用。服务器本地磁盘只做缓存，不是历史数据唯一来源。

## 11. Rust 迁移验收标准

在新增真实交易能力前，必须完成以下验收：

1. 用固定 UTC 1m K 线逐根比较 Python 和 Rust：信号方向、确认时间、入场价、止损价、过期时间和撤单原因一致。
2. 比较回测：成交数量、成交时间、止盈/止损/强平原因、保守/乐观权益和最大回撤一致。
3. USDT 与 USDC 钱包、费用和 PnL 全链路不混账；多资产模式有独立折算测试。
4. `TRADIFI_PERPETUAL` 使用独立交易时段、费率和结算资产，不因合约名称默认零费率。
5. 覆盖行情过期、断线恢复、重复成交事件、提交超时、撤单未知、重启恢复、保护单缺失和对账不一致。
6. 回测和模拟盘不依赖真实账户；网络集成只读并带超时。
7. 只有模拟盘连续运行、订单恢复和告警验收通过，并且用户明确授权后，才另立任务设计真实下单。

## 12. 当前实现边界

- 当前 Rust 项目只做公开行情、信号、模拟开仓和模拟退出，不签名请求，不提交真实订单。
- 当前撮合基于 K 线触价，不模拟真实订单簿排队、部分成交、资金费、抵押品自动兑换或实际强平撮合。
- 当前默认单进程、单交易对运行；扩展到约 10 个交易对时，必须为每个交易对保留独立 instrument、结算资产和状态命名空间，并在容量测试后再并发运行。
- API Key、DeepSeek Key 等凭据由云服务器 Secret Manager 注入 Rust 进程内存，不能进入 React、R2 回测结果、D1 业务字段或日志。

