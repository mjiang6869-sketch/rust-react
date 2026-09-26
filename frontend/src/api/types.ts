// 与 Rust 后端的 DTO 一一对应的类型。
//
// # 为什么价格与数量是 string 而不是 number
//
// JavaScript 的 number 是 IEEE 754 双精度浮点，`3200.12345678` 这样的价格
// 会丢精度。而做市的止盈目标是 bp 级（约 0.0001），精度丢失会让界面显示的价
// 与后端将要挂出的价不一致——用户看到的不是即将发生的事。
//
// 所以所有数值都以字符串传输。需要算术时用 `decimal.ts` 里的定点工具，
// 不要直接 `Number(...)`。

/** 统一响应包装。 */
export type ApiResponse<T> =
  | { status: 'ok'; data: T }
  | { status: 'error'; code: string; message: string }

export type Side = 'BUY' | 'SELL'

export type OrderPurpose = 'ENTRY' | 'TAKE_PROFIT' | 'STOP_LOSS'

export type ContractKind = 'CRYPTO_PERPETUAL' | 'TRADFI_PERPETUAL'

export type FeeSource =
  | 'EXCHANGE_ACCOUNT'
  | 'EXCHANGE_RULES'
  | 'PROMOTIONAL_ASSUMED'
  | 'CONFIGURED_DEFAULT'

export interface Health {
  ok: boolean
  version: string
  schema_version: number
}

/** 合约规则。 */
export interface InstrumentInfo {
  symbol: string
  contract_type: ContractKind
  base_asset: string
  quote_asset: string
  margin_asset: string
  /** 盈亏与手续费记入的资产。与 margin_asset 可能不同，绝不能相加。 */
  settlement_asset: string
  tick_size: string
  step_size: string
  min_qty: string
  min_notional: string
  /** 维持保证金率（百分比）。用于展示止损距强平的缓冲。 */
  maint_margin_pct: string
  maker_rate: string
  taker_rate: string
  fee_source: FeeSource
  /** 费率是否来自交易所对账。false 时结果不可用于决策。 */
  fee_is_authoritative: boolean
}

/** 一档止盈的状态。 */
export interface RungView {
  rung: number
  /** 给用户看的序号，从 1 开始。 */
  index: number
  pct: string
  distance_bp: string
  fraction: string
  price: string
  filled: boolean
}

export interface PositionInfo {
  symbol: string
  side: Side
  side_label: string
  quantity: string
  entry_price: string
  unrealized_pnl: string
  stop_price: string | null
  /**
   * 止损已触发但未成交——仓位正在裸露。
   *
   * maker-only 特有风险：止损是挂单，跳空穿过它且不回来时没有任何机制会
   * 平掉仓位。界面必须醒目提示。
   */
  stop_triggered: boolean
  rungs: RungView[]
  realized_pnl: string
}

/** 这张单是谁下的。`null` 只出现在数据不一致的边缘情形。 */
export type OrderSource = 'MANUAL' | 'STRATEGY'

export interface OrderInfo {
  client_id: string
  purpose: OrderPurpose
  purpose_label: string
  side: Side
  quantity: string
  limit_price: string
  filled: string
  state: string
  source: OrderSource | null
  source_label: string | null
  /** 是否可撤销（目前只有在途开仓单可撤）。 */
  cancellable: boolean
  /** 到期自动撤销的时刻（仅在途开仓单有值）。 */
  expires_at: string | null
}

export interface SafetyInfo {
  armed: boolean
  user_stream_connected: boolean
  account_reconciled: boolean
  blocking_reasons: string[]
}

/** 自动化做市（区间做市策略）的运行状态标签。 */
export type AutoMakerStatus =
  | 'DISABLED'
  | 'QUOTING'
  | 'IN_POSITION'
  | 'YIELDING_TO_MANUAL'
  | 'FEED_DOWN'
  | 'WARMING_UP'
  | 'STANDING_DOWN'
  | 'READY'

/**
 * 自动化做市的可编辑参数。
 *
 * 数值一律是字符串（与全局约束一致）；`side_mode` 是互斥的字符串常量。
 */
export interface AutoMakerParams {
  lookback: string
  take_profit_bp: string
  stop_buffer_bp: string
  side_mode: 'LONG_ONLY' | 'SHORT_ONLY'
  equity_pct: string
  leverage: string
  valid_minutes: string
}

/** 自动化做市的开关、参数与运行状态。 */
export interface AutoMakerState {
  enabled: boolean
  strategy_id: string
  strategy_name: string
  status: AutoMakerStatus
  status_label: string
  params: AutoMakerParams
  warmup_have: number
  warmup_need: number
  max_lookback: number
}

/** `GET/PUT /api/v1/auto-maker` 的响应：运行状态 + 可编辑字段说明。 */
export interface AutoMakerConfig extends AutoMakerState {
  fields: ParameterInfo[]
}

export interface EngineState {
  mode: 'PAPER' | 'LIVE'
  mode_label: string
  symbol: string
  initial_equity: string
  equity: string
  realized_pnl: string
  unrealized_pnl: string
  total_fees: string
  position: PositionInfo | null
  open_orders: OrderInfo[]
  feed_connected: boolean
  feed_fresh: boolean
  last_event_at: string | null
  /** 策略让位原因。非 null 时说明策略当前不交易。 */
  stand_down: string | null
  /** 成交模型名称与乐观度。用户需要知道当前结论建立在哪种假设上。 */
  fill_model: string
  fill_model_optimism: string
  safety: SafetyInfo
  instrument: InstrumentInfo
  /** 自动化做市（区间做市策略）的开关、参数与运行状态。 */
  auto_maker: AutoMakerState
  /** 当前持仓的来源（手动 / 自动化做市）；无持仓时为 `null`。 */
  position_source: OrderSource | null
}

/** 本次模拟盘运行的账户收益快照；金额由后端按 Decimal 计算。 */
export interface OverviewData {
  source: 'paper_account_snapshots'
  symbol: string
  settlement_asset: string
  as_of: string
  session_started_at: string
  equity: string
  cumulative_pnl: string
  cumulative_return_pct: string | null
  realized_pnl: string
  unrealized_pnl: string
  estimated_month_pnl: string | null
  estimated_annualized_pct: string | null
  fee_is_authoritative: boolean
  sample_days: number
  curve: Array<{ at: string; equity: string }>
  daily: Array<{ date: string; realized_pnl: string; intensity: number }>
}

export interface ParameterInfo {
  key: string
  label: string
  description: string
  unit: string | null
  default: string
  min: string
  max: string
  /** 该参数存的是比例，显示时要乘 100。 */
  display_as_percent: boolean
}

export interface StrategyInfo {
  id: string
  name: string
  warmup_candles: number
  parameters: ParameterInfo[]
}

export interface FillModelInfo {
  key: string
  name: string
  data_requirements: string
  optimism: 'UPPER_BOUND' | 'CONSERVATIVE_LOWER'
  optimism_note: string
}

/** 一档止盈的预览。 */
export interface RungPreview {
  rung: number
  index: number
  price: string
  quantity: string
  gross_profit: string
  distance_bp: string
}

/** 手动下单预览。这里的价位就是将要挂出的价位。 */
export interface ManualPreview {
  entry: string
  stop: string
  quantity: string
  notional: string
  margin_required: string
  liquidation_buffer_pct: string | null
  take_profits: RungPreview[]
  accepted: boolean
  reject_reason: string | null
  warnings: string[]
}

/** 手动下单请求。 */
export interface ManualPlanRequest {
  symbol: string
  side: Side
  entry: string
  quantity?: string
  size_pct?: string
  leverage: string
  stop?: string
  stop_distance_bp?: string
  take_profit?: Array<{ pct: string; fraction: string }>
  take_profit_pct?: string
  take_profit_prices?: Array<{ price: string; fraction: string }>
  take_profit_price?: string
  break_even?: { trigger_r: string; offset: string }
  trailing?: { distance: string; activate_at?: string }
  cancel_unfilled_after_secs?: number
  client_ref?: string
}

export interface ModelResult {
  name: string
  optimism: string
  final_equity: string
  pnl: string
  trade_count: number
  win_rate: string | null
}

/** 结论可信度。这是回测最重要的输出。 */
export interface BacktestVerdict {
  conclusive: boolean
  message: string
  sign_flips: boolean
  breakeven_fill_rate: string | null
  markout_5s: string | null
  fee_incomplete: boolean
  stop_exposure_events: number
  max_exposure_secs: number
  pnl_at_promotional_fee: string
  pnl_at_standard_fee: string
}

export interface BacktestResult {
  symbol: string
  strategy_id: string
  from: string
  to: string
  candle_count: number
  models: ModelResult[]
  verdict: BacktestVerdict
}

export interface BacktestRunSummary {
  run_id: string
  symbol: string
  strategy_id: string
  fill_model: string
  initial_equity: string
  final_equity: string
  trade_count: number
  sign_flips: boolean
  breakeven_fill_rate: string | null
  adverse_markout_5s: string | null
  fee_incomplete: boolean
  actionable: boolean
  verdict: string
}

export interface DatasetCoverage {
  kind: string
  symbol: string
  partitions: number
  finalized: number
  first_month: string | null
  last_month: string | null
  parquet_bytes: number
  problems: string[]
}

export interface DataGap {
  kind: string
  symbol: string
  from: string
  to: string
  note: string
}

export interface Coverage {
  data_root: string
  datasets: DatasetCoverage[]
  gaps: DataGap[]
}

export interface DownloadRequest {
  symbols: string[]
  kinds: string[]
  from: string
  to: string
}

/** 下载任务状态机的状态标签。 */
export type DownloadJobState = 'idle' | 'running' | 'finished' | 'cancelled' | 'failed'

/** 下载请求的回显（供快照展示，不做二次校验）。 */
export interface DownloadJobRequest {
  symbols: string[]
  kinds: string[]
  from: string
  to: string
}

/** 裁剪后的下载计划摘要。 */
export interface DownloadJobPlan {
  total: number
  /** 因归档范围裁剪掉的说明（小字展示）。 */
  clipped: string[]
  /** 是否成功取得归档范围。false 时计划是按“到上个月”兜底算出的。 */
  index_available: boolean
}

/** 正在处理的分区。 */
export interface DownloadJobCurrent {
  symbol: string
  kind: string
  month: string
  stage: string
  stage_label: string
  stage_done: number
  /** `null` 表示这个阶段没有已知总量（例如下载中，只能看已处理字节）。 */
  stage_total: number | null
  stage_started_at: string
}

/** 一个分区的失败记录。 */
export interface DownloadJobFailure {
  partition: string
  error: string
}

/** 下载任务的完整状态快照。WebSocket 推送与 REST 查询共用同一形状。 */
export interface DownloadJob {
  state: DownloadJobState
  started_at: string | null
  finished_at: string | null
  request: DownloadJobRequest | null
  plan: DownloadJobPlan | null
  done: number
  completed: number
  not_in_archive: number
  failed: number
  current: DownloadJobCurrent | null
  failures: DownloadJobFailure[]
  last_error: string | null
}

/** 某数据集在币安归档（S3）里的覆盖范围。 */
export interface ArchiveDataset {
  kind: string
  label: string
  earliest: string | null
  latest: string | null
  months: number
  error: string | null
}

/** `GET /api/v1/data/archive-range` 的响应。 */
export interface ArchiveRange {
  symbol: string
  source: string
  fetched_at: string
  hint: string
  datasets: ArchiveDataset[]
}

/** 一笔成交。 */
export interface FillRecord {
  trade_id: string
  client_order_id: string
  quantity: string
  price: string
  fee: string
  /** 手续费记入的结算资产。USDT 与 USDC 必须分开。 */
  fee_asset: string
  at: string
}

/** WebSocket 服务端消息。 */
export type ServerMessage =
  | { type: 'snapshot'; channel: string; data: unknown }
  | { type: 'update'; channel: string; data: unknown }
  | { type: 'progress'; data: ProgressMessage }
  | { type: 'error'; code: string; message: string }
  | { type: 'pong' }

export type ProgressMessage =
  | { type: 'download_status'; job: DownloadJob }
  | { type: 'backtest'; symbol: string; model: string; done: number; total: number }

/** K 线。时间用秒级时间戳，与 lightweight-charts 的要求一致。 */
export interface CandleBar {
  time: number
  open: number
  high: number
  low: number
  close: number
  volume: number
}

// ---------------------------------------------------------------------------
// 行情（图表、盘口、成交流）
// ---------------------------------------------------------------------------

/**
 * 一根 K 线。
 *
 * `time` 是 **Unix 秒**（不是毫秒）——`lightweight-charts` 的 `UTCTimestamp`
 * 以秒为单位。毫秒传进去会让图表把时间解释到公元 5 万年。
 *
 * 价格是字符串：bp 级的数值经不起 `number` 的精度损失。但图表库要求
 * `number`——所以**只在这一个地方**转换，且转换前后不参与任何算术
 * （见 `chart/ChartHost.tsx`）。
 */
export interface CandleBar {
  time: number
  open: number
  high: number
  low: number
  close: number
  volume: number
  /** 是否已收盘。未收盘的是当前正在形成的 K 线，每刻都在变。 */
  closed: boolean
}

/** 原始 K 线（价格是字符串）。`ChartHost` 之外不要用。 */
export interface RawCandle {
  time: number
  open: string
  high: string
  low: string
  close: string
  volume: string
  closed: boolean
  /** 后端计算：收减开、相对开盘价的百分数；兼容旧服务时显示占位符。 */
  change?: string
  change_percent?: string | null
}

export interface KlinesResponse {
  symbol: string
  interval: string
  candles: RawCandle[]
  /**
   * 数据来源。
   *
   * 「币安公开行情」与「本地归档」不是同一份数据：回测读本地归档，图表读
   * 这里。不显示出来，用户会以为图表覆盖的区间就是回测覆盖的区间。
   */
  source: string
}

export interface BookLevel {
  price: string
  quantity: string
  /** 该档累计量（后端算好，前端不累加——浮点误差会让深度条宽度失真）。 */
  cumulative: string
}

export interface BookSnapshotResponse {
  symbol: string
  bid: string
  ask: string
  mid: string
  spread: string
  /** 价差相对中间价的基点。做市看这个数，不看绝对值。 */
  spread_bp: string
  bids: BookLevel[]
  asks: BookLevel[]
}

export interface StreamCandle extends RawCandle {
  /** 交易所事件时间，毫秒字符串。 */
  event_ms: string
}

export interface KlineFrame {
  interval: string
  live: boolean
  notice: string | null
  generation: string
  /** 按开盘时间升序，包含最近收盘帧。 */
  candles: StreamCandle[]
}

/**
 * 行情推送的一帧（`/api/v1/market/stream`）。
 *
 * 每帧都是**完整视图**：直接替换本地状态，不做合并。
 */
export interface MarketFrame {
  type: 'market'
  kline?: KlineFrame
  symbol: string
  /** 数据来源，界面必须显示。 */
  source: string
  /** 盘口与成交两条上游都在收数据。 */
  live: boolean
  /** 只是还在建立连接（没有断线、没有限流）。 */
  connecting: boolean
  /** 不在线时的原因。`null` 表示一切正常。 */
  notice: string | null
  /** 上游限流的剩余冷却毫秒数。0 表示没有冷却。 */
  cooldown_ms: number
  book: BookSnapshotResponse | null
  /** 最新一笔成交的价格。连上后还没有任何成交时为 `null`——不用 REST 补底。 */
  last_price: string | null
}

/** 图表支持的周期。与后端 `Interval` 一一对应。 */
export const INTERVALS = [
  { value: '1m', label: '1分' },
  { value: '3m', label: '3分' },
  { value: '5m', label: '5分' },
  { value: '15m', label: '15分' },
  { value: '30m', label: '30分' },
  { value: '1h', label: '1时' },
  { value: '4h', label: '4时' },
  { value: '1d', label: '日线' },
] as const

export type IntervalValue = (typeof INTERVALS)[number]['value']
