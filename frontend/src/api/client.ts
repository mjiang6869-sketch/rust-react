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

import type {
  ApiResponse,
  BacktestResult,
  BacktestRunSummary,
  Coverage,
  DownloadRequest,
  EngineState,
  FillModelInfo,
  FillRecord,
  Health,
  ManualPlanRequest,
  ManualPreview,
  StrategyInfo,
} from './types'

/** API 错误。`message` 是可直接展示的中文。 */
export class ApiError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly status: number,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

const BASE = '/api/v1'

async function request<T>(
  path: string,
  init?: RequestInit & { idempotencyKey?: string },
): Promise<T> {
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
  return parsed.data
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
