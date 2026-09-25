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

export interface OrderInfo {
  client_id: string
  purpose: OrderPurpose
  purpose_label: string
  side: Side
  quantity: string
  limit_price: string
  filled: string
  state: string
}

export interface SafetyInfo {
  armed: boolean
  user_stream_connected: boolean
  account_reconciled: boolean
  blocking_reasons: string[]
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
  stop: string
  take_profit?: Array<{ pct: string; fraction: string }>
  take_profit_pct?: string
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
  | {
      type: 'download'
      symbol: string
      kind: string
      month: string
      done: number
      total: number
    }
  | { type: 'download_done'; completed: number; failed: number }
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
