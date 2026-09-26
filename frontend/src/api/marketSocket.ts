// 盘口与成交流的推送连接（`/api/v1/market/stream`）。
//
// # 为什么单独一个文件、不放在 hook 里
//
// 与 `poller.ts` 同一个理由：连接的生命周期（连、断、退避、重连、停）是
// 事故里最容易出"多一条"的地方——多一条轮询链让权重翻倍，多一条推送连接
// 同样会让后端多一个订阅者、让界面收到两份帧。这里不碰 React、不碰 DOM，
// 只依赖注入进来的 `open`/`setTimeout`/`clearTimeout`，所以离线脚本可以用
// 假时钟直接数"同时开着几条连接"。
//
// # 纪律
//
// 1. **任何时刻最多一条连接**。唯一的建连入口是 `connect`，它在已有连接或
//    已排定重连时直接返回。
// 2. **断线重连要退避**。指数退避带抖动，上限 30 秒。收到第一帧才复位——
//    "连上立刻被断"不算恢复，否则会退化成每秒重连一次。
// 3. **旧连接的事件一律作废**。`stop` 或重连之后，旧连接迟到的 `onclose`
//    不能再触发一次重连（那就是第二条链）。每条连接带一个身份，事件回调先
//    核对身份。

import type { MarketFrame } from './types'

/** 首次重连等待。 */
export const INITIAL_BACKOFF_MS = 1_000
/** 重连等待上限。 */
export const MAX_BACKOFF_MS = 30_000
/** 心跳间隔：有些代理会在空闲时断开连接。 */
export const PING_MS = 20_000

/** 一条连接收到的事件。 */
export interface ConnectionHandlers {
  onOpen(): void
  onMessage(text: string): void
  onClose(): void
}

/** 一条已建立（或正在建立）的连接。 */
export interface Connection {
  send(text: string): void
  close(): void
}

/** 注入的环境。浏览器用 `browserDeps`，离线检查用假实现。 */
export interface SocketDeps {
  open(url: string, handlers: ConnectionHandlers): Connection
  setTimeout(fn: () => void, ms: number): ReturnType<typeof setTimeout>
  clearTimeout(handle: ReturnType<typeof setTimeout>): void
  /** `[0, 1)` 的随机数，用于抖动。 */
  random(): number
}

export interface MarketSocketOptions {
  url: string
  /** 只接受这个交易对的帧；切换交易对期间迟到的旧帧被丢弃。 */
  symbol: string
  deps: SocketDeps
  /** 收到一帧完整行情。 */
  onFrame(frame: MarketFrame): void
  /** 与服务端的连接断开，将在 `retryInMs` 后重连。 */
  onDisconnect(retryInMs: number): void
}

export interface MarketSocket {
  start(): void
  /** 停止：关连接、取消重连，之后不会再有任何回调。 */
  stop(): void
  /** 当前开着的连接数。正常情况下只可能是 0 或 1。 */
  readonly openConnections: number
  /** 下一次重连的等待（毫秒）。 */
  readonly backoffMs: number
}

export function createMarketSocket(opts: MarketSocketOptions): MarketSocket {
  const { deps } = opts

  let current: { conn: Connection | null } | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let pingTimer: ReturnType<typeof setTimeout> | null = null
  let backoff = INITIAL_BACKOFF_MS
  let stopped = false

  const clearPing = (): void => {
    if (pingTimer !== null) {
      deps.clearTimeout(pingTimer)
      pingTimer = null
    }
  }

  const schedulePing = (conn: Connection): void => {
    clearPing()
    pingTimer = deps.setTimeout(() => {
      pingTimer = null
      conn.send(JSON.stringify({ op: 'ping' }))
      schedulePing(conn)
    }, PING_MS)
  }

  const scheduleReconnect = (): void => {
    if (stopped || reconnectTimer !== null) return
    const jitter = deps.random() * backoff * 0.3
    const delay = Math.min(backoff + jitter, MAX_BACKOFF_MS)
    backoff = Math.min(backoff * 2, MAX_BACKOFF_MS)
    opts.onDisconnect(delay)
    reconnectTimer = deps.setTimeout(() => {
      reconnectTimer = null
      connect()
    }, delay)
  }

  /** **唯一的建连入口。** */
  const connect = (): void => {
    if (stopped || current !== null || reconnectTimer !== null) return

    // 身份：回调里核对 `current === me`，旧连接的迟到事件因此失效。
    const me: { conn: Connection | null } = { conn: null }
    current = me

    const handlers: ConnectionHandlers = {
      onOpen(): void {
        if (current !== me || me.conn === null) return
        schedulePing(me.conn)
      },
      onMessage(text): void {
        if (current !== me) return
        let msg: unknown
        try {
          msg = JSON.parse(text)
        } catch {
          return
        }
        if (!isMarketFrame(msg) || msg.symbol !== opts.symbol) return
        // 收到帧才算恢复，见文件头第 2 条。
        backoff = INITIAL_BACKOFF_MS
        opts.onFrame(msg)
      },
      onClose(): void {
        if (current !== me) return
        current = null
        clearPing()
        scheduleReconnect()
      },
    }

    try {
      me.conn = deps.open(opts.url, handlers)
    } catch {
      current = null
      scheduleReconnect()
    }
  }

  return {
    start: connect,
    stop(): void {
      stopped = true
      if (reconnectTimer !== null) {
        deps.clearTimeout(reconnectTimer)
        reconnectTimer = null
      }
      clearPing()
      const conn = current?.conn ?? null
      current = null
      conn?.close()
    },
    get openConnections(): number {
      return current === null ? 0 : 1
    },
    get backoffMs(): number {
      return backoff
    },
  }
}

function isMarketFrame(v: unknown): v is MarketFrame {
  return (
    typeof v === 'object' &&
    v !== null &&
    (v as { type?: unknown }).type === 'market' &&
    typeof (v as { symbol?: unknown }).symbol === 'string' &&
    Array.isArray((v as { trades?: unknown }).trades)
  )
}

/** 推送地址。与引擎状态的 `/api/v1/ws` 同源，开发时经 Vite 代理。 */
export function marketStreamUrl(symbol: string, interval?: string): string {
  const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const q = new URLSearchParams({ symbol })
  if (interval !== undefined) q.set('interval', interval)
  return `${proto}//${window.location.host}/api/v1/market/stream?${q.toString()}`
}

/** 浏览器环境。 */
export const browserDeps: SocketDeps = {
  open(url, h): Connection {
    const ws = new WebSocket(url)
    ws.onopen = () => h.onOpen()
    ws.onmessage = (ev) => {
      if (typeof ev.data === 'string') h.onMessage(ev.data)
    }
    // onerror 之后一定紧跟 onclose，重连只在 onclose 里处理。
    ws.onclose = () => h.onClose()
    return {
      send(text): void {
        if (ws.readyState === WebSocket.OPEN) ws.send(text)
      },
      close(): void {
        ws.close()
      },
    }
  },
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (h) => clearTimeout(h),
  random: () => Math.random(),
}
