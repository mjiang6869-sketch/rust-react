# domain

交易域核心。**零 I/O**，这是硬约束而非风格偏好。

## 允许的依赖

只允许：`rust_decimal`、`chrono`、`serde`、`thiserror`。

禁止 `tokio`、`reqwest`、`duckdb`、`axum`、任何网络或文件库。CI 校验：

```sh
cargo tree -p domain | grep -E 'tokio|reqwest|duckdb|axum'   # 必须无输出
```

## 为什么这条约束重要

回测、模拟盘、实盘必须共用**同一套**订单生命周期与保护单数学。旧实现把它们
写成三份独立实现，后果不是"代码重复"这种审美问题，而是已经发生的功能性缺陷：

- **止盈价舍入方向在三个文件里不一致**（`backtest.rs` 对多空都用向下取整，
  `paper.rs`/`live.rs` 对买入用向上取整），导致回测在实盘永远不会挂出的价格上
  成交多单止盈。止盈目标是 bp 级时，这个误差足以让回测结论失效。
- **维持保证金率硬编码为 0.4%**，真实值是 2.5%（`exchangeInfo` 的
  `maintMarginPercent`）。用它判断止损安全性会让策略在任何像样的杠杆下静默
  拒绝信号。

零 I/O 让整个 crate 可被纯函数测试覆盖：55 个测试全部无 fixture、无网络、
无 tokio runtime。上面两个缺陷在新结构下都有一条直接对应的测试。

## 模块

| 模块 | 职责 |
| --- | --- |
| `money` | `Price` / `Qty` 新类型，防止参数顺序错误，序列化为字符串 |
| `precision` | 方向感知量化。**舍入方向由 `PriceRole` 唯一决定，调用方无选择权** |
| `instrument` | 合约规则、费率快照、强平价估算（读真实维持保证金率） |
| `order` | 订单、状态机状态、副作用 `Effect` |
| `state` | `OrderBookState`：订单与持仓的唯一权威 |
| `protection` | 止盈/止损/保本/移动止损/分批止盈/定时取消的唯一实现 |
| `error` | 按**可恢复性**分类的交易所错误 |

## 两条不可违反的不变量

1. **交易数值禁止浮点。** `clippy.toml` 的 `disallowed-types` 在编译期强制
   `f32`/`f64` 不能出现在本 crate。
2. **保护单价格不由调用方手选舍入方向。** 见 `precision::PriceRole` 的契约表。
   止盈与止损在同一订单方向上的取整**相反**（止盈多赚一跳，止损早出一跳），
   这不是笔误。
