# Rust Crypto 云端架构与部署方案

本文是 rust-crypto 的云端部署基线，适用于 Rust 交易进程部署在云服务器、React 前端部署在 Cloudflare Pages、历史行情和回测结果全部保存在云端的方案。

## 1. 架构总览

```text
React / Cloudflare Pages
        |
        v
Cloudflare Worker API
   |       |        |
   v       v        v
  D1      R2      Queues
元数据   大文件   异步任务
        |
        v
云服务器 Rust 服务
  |       |       |
  v       v       v
Secret   Binance  R2/D1
Manager  API      数据与任务
```

| 数据 | 服务 | 用途 |
| --- | --- | --- |
| 原始 ZIP/CSV、规范化 K 线、逐笔成交、L2、回测结果 | R2 | 大文件和 Parquet，按月分区 |
| 数据集索引、覆盖范围、缺口、checksum、任务状态 | D1 | 只保存元数据，不保存每根 K 线 |
| 下载和回测任务 | Queues | 异步投递、重试、死信 |
| 实时连接和单实例协调 | Durable Objects（可选） | 不保存历史行情 |
| Binance、DeepSeek 密钥 | 云服务器 Secret Manager | 注入 Rust 进程内存 |

R2 保存权威行情和回测文件，D1 保存索引。不要把数千万根 K 线逐行写入 D1。当前约 10 个交易对不需要 Redis、ClickHouse 或 Iceberg。

## 2. Cloudflare 资源创建

以下命令需要先安装 Wrangler 并登录 Cloudflare：

```sh
pnpm add -g wrangler
wrangler login
wrangler whoami
```

### 2.1 创建 R2

```sh
wrangler r2 bucket create rust-crypto-market-data-prod
wrangler r2 bucket create rust-crypto-backtests-prod
```

对象路径：

```text
market-data/v1/raw/venue=binance/product=usd-m/symbol=BTCUSDT/interval=1m/year=2025/month=01/source.zip
market-data/v1/curated/venue=binance/product=usd-m/symbol=BTCUSDT/interval=1m/year=2025/month=01/part-000.parquet
market-data/v1/manifests/dataset=klines/product=usd-m/symbol=BTCUSDT/interval=1m/year=2025/month=01.json
backtests/v1/{run_id}/summary.json
backtests/v1/{run_id}/trades.parquet
```

### 2.2 创建 D1

```sh
wrangler d1 create rust-crypto-meta-prod
```

把返回的 database_id 写入 Worker 的实际部署配置。建议表：

```sql
CREATE TABLE instruments (
  instrument_id TEXT PRIMARY KEY,
  venue TEXT NOT NULL,
  symbol TEXT NOT NULL,
  product_type TEXT NOT NULL,
  base_asset TEXT NOT NULL,
  quote_asset TEXT NOT NULL,
  margin_asset TEXT NOT NULL,
  settlement_asset TEXT NOT NULL,
  contract_type TEXT NOT NULL,
  status TEXT NOT NULL,
  metadata_json TEXT NOT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  updated_at TEXT NOT NULL
);

CREATE TABLE market_datasets (
  dataset_id TEXT PRIMARY KEY,
  venue TEXT NOT NULL,
  product_type TEXT NOT NULL,
  symbol TEXT NOT NULL,
  interval TEXT NOT NULL,
  start_time_utc TEXT NOT NULL,
  end_time_utc TEXT NOT NULL,
  row_count INTEGER NOT NULL,
  object_prefix TEXT NOT NULL,
  checksum TEXT NOT NULL,
  schema_version TEXT NOT NULL,
  completeness TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE market_data_gaps (
  dataset_id TEXT NOT NULL,
  gap_start_utc TEXT NOT NULL,
  gap_end_utc TEXT NOT NULL,
  gap_type TEXT NOT NULL,
  PRIMARY KEY (dataset_id, gap_start_utc)
);

CREATE TABLE data_jobs (
  job_id TEXT PRIMARY KEY,
  job_type TEXT NOT NULL,
  status TEXT NOT NULL,
  request_json TEXT NOT NULL,
  result_object_prefix TEXT,
  error_code TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE backtest_runs (
  run_id TEXT PRIMARY KEY,
  dataset_id TEXT NOT NULL,
  strategy_version TEXT NOT NULL,
  config_hash TEXT NOT NULL,
  result_object_prefix TEXT NOT NULL,
  status TEXT NOT NULL,
  started_at TEXT,
  completed_at TEXT,
  created_at TEXT NOT NULL
);

CREATE INDEX idx_market_datasets_lookup ON market_datasets (venue, product_type, symbol, interval, start_time_utc);
CREATE INDEX idx_data_jobs_status ON data_jobs (status, created_at);
```

初始化迁移：

```sh
wrangler d1 execute rust-crypto-meta-prod --remote --file=./infra/d1/001_initial.sql
```

### 2.3 创建 Queues

```sh
wrangler queues create rust-crypto-download-prod
wrangler queues create rust-crypto-backtest-prod
wrangler queues create rust-crypto-dead-letter-prod
```

队列消息必须包含唯一 job_id。Queues 默认至少一次投递，消费者必须幂等。

## 3. Worker 绑定示例

```toml
name = "rust-crypto-api-prod"
main = "src/index.ts"
compatibility_date = "2026-09-25"

[[d1_databases]]
binding = "META_DB"
database_name = "rust-crypto-meta-prod"
database_id = "<actual-database-id>"

[[r2_buckets]]
binding = "MARKET_DATA"
bucket_name = "rust-crypto-market-data-prod"

[[r2_buckets]]
binding = "BACKTESTS"
bucket_name = "rust-crypto-backtests-prod"

[[queues.producers]]
binding = "DOWNLOAD_QUEUE"
queue = "rust-crypto-download-prod"

[[queues.producers]]
binding = "BACKTEST_QUEUE"
queue = "rust-crypto-backtest-prod"

[vars]
ENVIRONMENT = "production"
RUST_API_BASE_URL = "https://trade.example.com"
```

API Key、Secret、数据库密码和签名密钥不能写入 vars。

## 4. 密钥管理

Rust 运行在云服务器时，优先使用 AWS Secrets Manager、GCP Secret Manager、Azure Key Vault 或 HashiCorp Vault。使用实例角色或工作负载身份读取密钥，不在磁盘保存 .env，不把 Secret 打进镜像。

建议密钥名称：

```text
rust-crypto/prod/binance/paper/api-key
rust-crypto/prod/binance/paper/api-secret
rust-crypto/prod/binance/live/api-key
rust-crypto/prod/binance/live/api-secret
rust-crypto/prod/deepseek/api-key
```

启动流程：实例身份认证 -> Secret Manager -> 仅注入 Rust 内存 -> 检查 PAPER/LIVE 和 endpoint allowlist -> 启动。密钥不能写日志、metrics、HTTP 响应或回测结果；Binance Key 禁止提现权限，LIVE Key 开启 IP 白名单。

Cloudflare Workers Secrets 适合 Worker 自己调用外部 API，不应通过公开接口把 Binance Secret 返回给 Rust。

## 5. Rust 连接方式

R2 使用 S3 兼容 API。Rust 需要 GetObject、PutObject、multipart upload、checksum 和 Range 读取能力，凭据由 Secret Manager 提供。

D1 不作为公网 SQLite 直接连接。Rust 通过受保护 Worker 内部 API 访问：

```text
Rust -> HTTPS mTLS / 服务令牌 -> Worker API -> D1
```

建议内部接口：

```text
GET  /internal/datasets
POST /internal/jobs/{job_id}/claim
POST /internal/jobs/{job_id}/heartbeat
POST /internal/jobs/{job_id}/complete
POST /internal/backtests/{run_id}/result
```

R2 文件必须先完成上传、checksum 校验，再写 D1 manifest；上传失败不能标记数据可用。

## 6. 数据规范

默认只保存 1m 作为权威 K 线，其他周期从 1m 确定性聚合。manifest 必须记录 venue、product_type、symbol、quote_asset、margin_asset、settlement_asset、interval、UTC 覆盖区间、row_count、checksum、schema_version、source 和缺口状态。

USDT、USDC 的 quote_asset、margin_asset、settlement_asset 必须分开。USDT 余额能否作为 USDC 合约保证金，属于账户模式和抵押品折算规则，不能在 K 线数据中合并。TradFi 还要记录交易时段、Maker/Taker 费率、资金费和结算资产，不能永久假设 Maker 为零。

做市回测后续还需单独保存 aggTrade、depth snapshot、depth diff、mark price、index price、funding rate、liquidation 和 exchangeInfo snapshot。

## 7. 任务流程

```text
下载：Worker 创建任务 -> Queues -> Rust 下载 -> R2 raw -> checksum -> Parquet -> R2 curated -> D1 manifest

回测：React 提交 -> D1 backtest_runs -> Queues -> Rust 读取 R2 -> 检查缺口 -> 回测 -> R2 结果 -> D1 摘要
```

回测结果必须绑定 dataset_id、schema_version、策略版本、配置 hash、交易对、结算资产和 UTC 时间范围。

## 8. 云服务器部署

Rust 服务使用 Docker 或 systemd。服务器磁盘只作为临时缓存，不作为历史数据唯一来源。入站限制 SSH 和必要健康检查，管理端口限制固定 IP 或 VPN；Rust 管理 API 通过 Cloudflare Tunnel 或 Worker 反代；出站只允许 Binance、R2、内部 Worker API 和 Secret Manager。

## 9. 实施阶段

1. 创建 R2、D1、Queues，实现 1m 下载、checksum、Parquet、manifest。
2. Rust 回测 worker 从 R2 读取数据，结果写 R2，摘要写 D1。
3. Rust 模拟盘部署云服务器，使用 Secret Manager，接入 Worker 内部 API。
4. 真实交易前完成账户对账、用户数据流、订单恢复、USDT/USDC 抵押品、TradFi 费率和订单簿回放。

## 10. 上线检查

- [ ] R2 原始和规范化数据都有 checksum 与 manifest。
- [ ] D1 只保存索引、任务和结果摘要。
- [ ] Rust 通过 Secret Manager 获取密钥，React 无法读取。
- [ ] PAPER/LIVE endpoint allowlist 已启用。
- [ ] Binance Key 禁止提现，LIVE Key 有 IP 白名单。
- [ ] 队列按 job_id 幂等。
- [ ] 回测记录数据集、策略版本和配置 hash。
- [ ] USDT、USDC、TradFi 结算资产没有混用。
- [ ] 日志、错误和 metrics 不包含密钥。
