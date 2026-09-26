// REST 客户端。
//
// # 与后端的契约
//
// 所有响应都是 `{ status: 'ok', data }` 或 `{ status: 'error', code, message }`。
// 错误消息是**面向用户的中文说明**，可以直接显示——后端已经写好了可读的
// 拒绝原因，前端不要用通用文案覆盖它。
//
// # 幂等键
//
// 所有写操作带 `Idempotency-Key`。手动下单面板一定会被双击，没有幂等保护
// 就会下出两张单。键由调用方生成（通常是「操作类型 + 时间戳 + 随机数」），
// 同一次用户操作必须复用同一个键。

import {
  armCooldown,
  clearCooldownIfElapsed,
  cooldownRemainingMs,
  parseRetryAfterMs,
} from './cooldown'
import type {
  ApiResponse,
  BacktestResult,
  BacktestRunSummary,
  Coverage,
  DownloadRequest,
  BookSnapshotResponse,
  EngineState,
  FillModelInfo,
  KlinesResponse,
  FillRecord,
  Health,
  ManualPlanRequest,
  ManualPreview,
  RecentTrade,
  StrategyInfo,
} from './types'

/**
 * API 错误。`message` 是可直接展示的中文。
 *
 * `retryAfterMs` 只在限流时存在——调用方据此安排退避。它是**服务端给的、
 * 与币安一致的**等待时间，不是前端猜的。
 */
export class ApiError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly status: number,
    readonly retryAfterMs?: number,
  ) {
    super(message)
    this.name = 'ApiError'
  }

  /** 是否是限流（HTTP 429）。 */
  get isRateLimited(): boolean {
    return this.status === 429
  }
}

/**
 * 限流冷却期间抛出的错误。
 *
 * 它与真正的 HTTP 429 区分开：这不是服务端拒绝了一次请求，而是**我们知道
 * 现在不该发**。界面上的措辞也不同——「正在限流等待」而不是「请求失败」。
 */
export class CooldownError extends ApiError {
  constructor(retryAfterMs: number) {
    super(
      'cooldown',
      `上游限流中，${Math.ceil(retryAfterMs / 1_000)} 秒后自动恢复`,
      429,
      retryAfterMs,
    )
    this.name = 'CooldownError'
  }
}

const BASE = '/api/v1'

async function request<T>(
  path: string,
  init?: RequestInit & { idempotencyKey?: string },
): Promise<T> {
  // 冷却期内**一个请求都不发**。这是把 429 拦在升级成 418 之前的关键：
  // 币安明确说过，超限后继续请求会延长封禁。
  const cooling = cooldownRemainingMs()
  if (cooling > 0) {
    throw new CooldownError(cooling)
  }

  const headers = new Headers(init?.headers)
  if (init?.body !== undefined) {
    headers.set('Content-Type', 'application/json')
  }
  if (init?.idempotencyKey !== undefined) {
    headers.set('Idempotency-Key', init.idempotencyKey)
  }

  let res: Response
  try {
    res = await fetch(`${BASE}${path}`, { ...init, headers })
  } catch (e) {
    // 网络层失败：后端可能没启动。
    throw new ApiError(
      'network',
      `无法连接后端服务。请确认 Rust 服务已启动（默认 127.0.0.1:8080）。${
        e instanceof Error ? `（${e.message}）` : ''
      }`,
      0,
    )
  }

  // 429 在**读取响应体之前**就要处理：`Retry-After` 是响应头，而且这个
  // 分支不需要正文。顺序反了头会被丢掉——这正是后端之前犯过的错。
  if (res.status === 429) {
    const retryAfterMs = parseRetryAfterMs(res.headers.get('Retry-After'))
    armCooldown(retryAfterMs)
    // 正文仍要读掉，否则连接可能不被复用
    await res.text().catch(() => undefined)
    throw new ApiError(
      'rate_limited',
      `上游限流，${Math.ceil(retryAfterMs / 1_000)} 秒后自动恢复`,
      429,
      retryAfterMs,
    )
  }

  let body: unknown
  const text = await res.text()
  try {
    body = JSON.parse(text)
  } catch {
    throw new ApiError(
      'bad_response',
      `后端返回了非 JSON 响应（HTTP ${res.status}）：${text.slice(0, 200)}`,
      res.status,
    )
  }

  const parsed = body as ApiResponse<T>
  if (parsed.status === 'error') {
    throw new ApiError(parsed.code, parsed.message, res.status)
  }
  if (parsed.status !== 'ok') {
    throw new ApiError('bad_response', `响应缺少 status 字段`, res.status)
  }
  // 成功即冷却已解除（`clearCooldownIfElapsed` 只在到期后才真的清）。
  clearCooldownIfElapsed()
  return parsed.data
}

/** 当前是否处于限流冷却中，以及剩余毫秒。界面据此显示状态。 */
export function currentCooldownMs(): number {
  return cooldownRemainingMs()
}

/** 生成幂等键。同一次用户操作必须复用同一个键。 */
export function newIdempotencyKey(action: string): string {
  const rand =
    typeof crypto !== 'undefined' && 'randomUUID' in crypto
      ? crypto.randomUUID()
      : Math.random().toString(36).slice(2)
  return `${action}-${Date.now()}-${rand}`
}

export const api = {
  health: () => request<Health>('/health'),

  state: () => request<EngineState>('/state'),

  strategies: () => request<StrategyInfo[]>('/strategies'),

  fillModels: () => request<FillModelInfo[]>('/fill-models'),

  /** 预览手动计划。**不改变引擎状态**。 */
  previewManual: (plan: ManualPlanRequest) =>
    request<ManualPreview>('/manual/preview', {
      method: 'POST',
      body: JSON.stringify(plan),
    }),

  /**
   * 提交手动计划。
   *
   * `idempotencyKey` 必须由调用方为同一次用户操作复用——双击时第二次请求
   * 会因键重复而被拒绝，而不是下出第二张单。
   */
  submitManual: (plan: ManualPlanRequest, idempotencyKey: string) =>
    request<ManualPreview>('/manual/submit', {
      method: 'POST',
      body: JSON.stringify(plan),
      idempotencyKey,
    }),

  cancelPending: () =>
    request<{ cancelled: boolean }>('/manual/cancel-pending', { method: 'POST' }),

  closePosition: () =>
    request<{ closed: boolean; pnl: string }>('/manual/close', { method: 'POST' }),

  /** 某交易对的成交历史。 */
  fills: (params: { symbol: string; from?: string; to?: string }) => {
    const q = new URLSearchParams({ symbol: params.symbol })
    if (params.from !== undefined) q.set('from', params.from)
    if (params.to !== undefined) q.set('to', params.to)
    return request<FillRecord[]>(`/fills?${q.toString()}`)
  },

  backtests: (symbol?: string) =>
    request<BacktestRunSummary[]>(
      `/backtests${symbol !== undefined ? `?symbol=${encodeURIComponent(symbol)}` : ''}`,
    ),

  runBacktest: (req: {
    symbol: string
    strategy?: string
    from: string
    to: string
    fill_models?: string[]
    initial_equity?: string
  }) =>
    request<BacktestResult>('/backtest', {
      method: 'POST',
      body: JSON.stringify(req),
    }),

  /**
   * K 线。
   *
   * `limit` 上限 1000（后端会 clamp）。历史数据从**币安公开 REST** 拉，
   * 不是本地归档——响应里带 `source` 说明来源，界面必须显示。
   */
  klines: (params: { symbol: string; interval: string; limit?: number; signal?: AbortSignal }) => {
    const q = new URLSearchParams({
      symbol: params.symbol,
      interval: params.interval,
    })
    if (params.limit !== undefined) q.set('limit', String(params.limit))
    return request<KlinesResponse>(`/market/klines?${q.toString()}`, params.signal ? { signal: params.signal } : undefined)
  },

  /** 盘口快照。 */
  book: (params: { symbol: string; limit?: number }) => {
    const q = new URLSearchParams({ symbol: params.symbol })
    if (params.limit !== undefined) q.set('limit', String(params.limit))
    return request<BookSnapshotResponse>(`/market/book?${q.toString()}`)
  },

  /** 最近成交。后端已按时间倒序（最新的在前）。 */
  recentTrades: (params: { symbol: string; limit?: number }) => {
    const q = new URLSearchParams({ symbol: params.symbol })
    if (params.limit !== undefined) q.set('limit', String(params.limit))
    return request<RecentTrade[]>(`/market/trades?${q.toString()}`)
  },

  coverage: () => request<Coverage>('/data/coverage'),

  startDownload: (req: DownloadRequest) =>
    request<{ started: boolean; message: string }>('/data/download', {
      method: 'POST',
      body: JSON.stringify(req),
    }),

  arm: () =>
    request<{ armed: boolean; blocking_reasons: string[] }>('/live/arm', {
      method: 'POST',
    }),

  disarm: () => request<{ armed: boolean }>('/live/disarm', { method: 'POST' }),

  setMode: (mode: 'PAPER' | 'LIVE') =>
    request<{ mode: string; mode_label: string }>('/mode', {
      method: 'PUT',
      body: JSON.stringify({ mode }),
    }),
}
