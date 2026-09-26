# data

历史行情数据的下载、校验、转换与台账。

## 数据来源

全部走 [`data.binance.vision`](https://data.binance.vision) 归档，不走 REST。
归档是 CDN 上的静态 ZIP，**无速率限制、支持 HTTP Range 断点续传**（已验证返回
`206 Partial Content`）。用 REST 拉多年历史会撞上 2400 权重/分钟的限制并有
封禁风险。

## 实测可用性（2026-09 确认）

BTCUSDC 与 ETHUSDC 的 1m 数据均从 **2024-01** 起，至 2026-08 共 32 个月。

| 数据集 | 覆盖 | 用途 |
| --- | --- | --- |
| `klines/1m` | 完整 | 策略信号 |
| `aggTrades` | 完整 | **M1 成交模型 + markout** |
| `markPriceKlines/1m` | 完整 | 强平距离估算 |
| `fundingRate` | 完整 | 持仓成本 |
| `trades` | 完整 | **不使用**：字段与 `aggTrades` 同构，体积 2.5 倍，不提供额外信息 |
| `bookTicker` | **2024-04 后停更** | **不可用**。markout 改用 `aggTrades` 成交价 |

### 为什么 `trades` 不用

两个数据集的字段几乎相同：

```text
aggTrades: agg_trade_id,price,quantity,first_trade_id,last_trade_id,transact_time,is_buyer_maker
trades:    id,price,qty,quote_qty,time,is_buyer_maker
```

都是只有 `is_buyer_maker` 一个方向标志，**都没有买卖双方的订单 ID**。
`trades` 只是把 `aggTrades` 里被合并的成交拆开，多出来的是"单笔填充"信息，
对做市回测无用。

### 排队位置无法从任何免费归档还原

这是必须接受的硬约束，不是可以靠更努力找数据解决的问题：

> `aggTrades` 和 `trades` 都只有 `is_buyer_maker`，没有订单 ID。
> `bookTicker` 已停更，且它本来也只有最优买卖价，不含队列深度。

所以 M1 成交模型"假设我们排在队尾"**不是保守的近似选择，而是唯一可能的
选择**。改进它只有一条路：从今天起自采实时 `depth@100ms` 流，攒够后做校准。

## 归档的两个坑

### 1. 不同数据集的 CSV 文件名会冲突

`klines/ETHUSDC/1m/ETHUSDC-1m-2026-08.zip` 与
`markPriceKlines/ETHUSDC/1m/ETHUSDC-1m-2026-08.zip` 解压后**都得到
`ETHUSDC-1m-2026-08.csv`**。

如果两类数据解压到同一目录，后者会静默覆盖前者，结果是拿标记价当成真实
K 线（特征是 `volume = 0`、`count = 60`，而真实 K 线 `count` 接近 1000）。
**解压路径必须按数据集分开。**

### 2. `HEAD` 请求不返回 `content-length`

S3 对这个 bucket 的 `HEAD` 响应不带 `content-length`，容易被误判为文件不存在。
判断文件是否存在与获取大小都用带 Range 的 `GET`：

```sh
curl -s -r 0-0 -o /dev/null -D - "$URL" | grep -i content-range
# -> content-range: bytes 0-0/348352540
```

### 3. K 线类路径多一层周期目录

K 线与标记价 K 线的归档路径是 `klines/ETHUSDC/1m/ETHUSDC-1m-2024-01.zip`，比
`aggTrades/ETHUSDC/ETHUSDC-aggTrades-2024-01.zip` 多一层 `1m/`。少这一层会 404，
而 404 又被当成「归档无此分区」的终态——v1 台账就是这样把全部 K 线误标的。
路径只在 `manifest::archive_prefix` 一处拼接，下载与列举共用。

### 4. 续传遇到 416

本地 ZIP 已完整时再发 `Range: bytes=N-`，服务端返回 **416**。它的意思是「本地已
完整」，不是错误：直接进入 sha256 校验，校验不符再删掉重下。

### 5. 列举必须用 S3 主机

`https://s3-ap-northeast-1.amazonaws.com/data.binance.vision?prefix=…&delimiter=/`
返回按 key 升序的 XML；同样的查询打到 `data.binance.vision` 只返回 HTML 页面。
列举用来发现每个数据集的最早/最晚月份（`archive_index`）。这个主机不带任何凭据，
**绝不能**加入 `exchange` 的签名白名单。

## 落盘格式

### 定点整数，不用字符串或浮点

| 表示 | 精度 | 体积 | 结论 |
| --- | --- | --- | --- |
| `f64` | 有误差 | 8 B | **禁止**——止盈目标是 bp 级时误差足以翻转结论 |
| 字符串 | 精确 | 8-20 B | 体积大，且列剪枝后需重新解析 |
| `i64` 定点 | 精确 | 8 B | **采用** |

币安的价格与数量最多 8 位小数（`pricePrecision`/`quantityPrecision` ≤ 8），
按 `1e8` 缩放可**无损**映射到 `i64`。资金费率用 `1e18`（它是 `0.00004296`
这种量级，`1e8` 会把有效数字压到 4 位）。

小数位超过 8 位时**报错而非截断**——静默丢精度正是要避免的事。

### 流式转换

单个 `aggTrades` 月的 CSV 是 891 MB。三标的 32 个月全量 CSV 超过 55 GB，
而 Parquet 后约 10-15 GB。先全部解压再统一转换会让峰值占用变成两者之和。

所以 `convert_csv_to_parquet` **每次只驻留一个批次**（10 万行）：逐行读 CSV
→ 攒满批次 → 写一个 row group → 释放。内存占用与输入大小无关。

实测（ETHUSDC 2026-08）：

| 数据集 | CSV | Parquet | 压缩比 | 耗时 |
| --- | --- | --- | --- | --- |
| `aggTrades` | 891 MB | 209 MB | 4.27x | 104s |
| `klines/1m` | 4.8 MB | 2.8 MB | 1.73x | 0.9s |
| `markPriceKlines/1m` | 4.3 MB | 1.6 MB | 2.65x | 0.7s |

## 台账

`Manifest` 是下载记忆，决定哪些分区可跳过、哪些有缺口。

- **`Finalized` 是核心状态**：校验通过后标记，永不重复下载。
- **行数与粒度期望不符时不标记通过**，记为 `Suspicious` 待查。K 线 1m 期望
  恰好 1440/天；资金费 3/天；逐笔成交**没有**期望值（不假装有）。
- **缺口必须阻断回测**：跨越缺口会凭空发明不可能的成交。
- **未知 `schema_version` 报错而非重置**：静默重置会丢失"已下载"的记忆。
- **v1 → v2 迁移**：两版磁盘结构相同，迁移只删除 K 线与标记价 K 线的
  `NotInArchive` 条目（v1 的 URL 错误导致的误标）。将来结构变化时须为 v1 单独
  保留反序列化结构。
- **写入用 `record_and_save`**：每次从磁盘重新加载、只合并一个分区再原子保存，
  避免多个任务各自整份保存互相覆盖。
- 解析时**先单独读版本号再解析结构**，这样格式变更时用户看到的是"版本不受
  支持"而不是误导性的"文件损坏"。

## 验证

```sh
cargo test -p data

# 用真实数据人工核对
cargo run -p data --example convert_one -- agg_trades \
  /path/to/ETHUSDC-aggTrades-2026-08.csv /tmp/out.parquet
```

已完成的真实数据验证：K 线与 aggTrades 的 Parquet 输出与原始 CSV **逐字段
一致**（含时间戳、`is_buyer_maker` 方向标志、全部 11 个数值列）。
