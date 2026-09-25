// WebSocket 客户端。
//
// # 两条必须做对的事
//
// ## 1. 断线重连要退避
//
// 后端重启或网络抖动时不能疯狂重连——那会给刚启动的服务造成冲击。用指数
// 退避，上限 30 秒，并带随机抖动（避免多个客户端同步重连）。
//
// ## 2. 快照与增量必须区分处理
//
// 后端在订阅后先发快照再发增量。快照是**完整状态**，增量也是完整状态
// （我们推全量而非差分），所以两者都直接替换本地状态即可。区分它们是为了
// 让重连后的第一个快照能覆盖掉断线期间的陈旧数据。
//
// # 为什么不用轮询
//
// 轮询在行情与订单这两个高频场景下既浪费带宽又让界面滞后。WebSocket 让
// 状态变更立刻可见——对做市尤其重要：止损触发未成交时仓位在裸露，延迟
// 几秒看到意味着几秒钟没有干预机会。

import type { EngineState, ProgressMessage, ServerMessage } from './types'

export interface WsHandlers {
  /** 收到完整状态（快照或增量）。 */
  onState?: (state: Partial<EngineState>) => void
  /** 后台任务进度。 */
  onProgress?: (progress: ProgressMessage) => void
  /** 连接状态变化。 */
  onConnectionChange?: (connected: boolean) => void
  /** 服务端返回的错误。 */
  onError?: (code: string, message: string) => void
}

const INITIAL_BACKOFF_MS = 500
const MAX_BACKOFF_MS = 30_000

export class EngineSocket {
  private ws: WebSocket | null = null
  private backoff = INITIAL_BACKOFF_MS
  private closed = false
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null
  private pingTimer: ReturnType<typeof setInterval> | null = null

  constructor(private readonly handlers: WsHandlers) {}

  connect(): void {
    if (this.closed) return
    if (this.ws !== null) return

    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const url = `${proto}//${window.location.host}/api/v1/ws`

    let socket: WebSocket
    try {
      socket = new WebSocket(url)
    } catch {
      this.scheduleReconnect()
      return
    }
    this.ws = socket

    socket.onopen = () => {
      this.backoff = INITIAL_BACKOFF_MS
      this.handlers.onConnectionChange?.(true)
      // 订阅。后端在收到订阅后再发一次快照，覆盖断线期间的陈旧状态。
      socket.send(JSON.stringify({ op: 'subscribe', channels: ['state', 'safety'] }))
      // 心跳：有些代理会在空闲时断开连接。
      this.pingTimer = setInterval(() => {
        if (socket.readyState === WebSocket.OPEN) {
          socket.send(JSON.stringify({ op: 'ping' }))
        }
      }, 20_000)
    }

    socket.onmessage = (ev) => {
      let msg: ServerMessage
      try {
        msg = JSON.parse(ev.data as string) as ServerMessage
      } catch {
        return
      }
      switch (msg.type) {
        case 'snapshot':
        case 'update':
          this.handlers.onState?.(msg.data as Partial<EngineState>)
          break
        case 'progress':
          this.handlers.onProgress?.(msg.data)
          break
        case 'error':
          this.handlers.onError?.(msg.code, msg.message)
          break
        case 'pong':
          break
      }
    }

    socket.onclose = () => {
      this.cleanupSocket()
      this.handlers.onConnectionChange?.(false)
      this.scheduleReconnect()
    }

    socket.onerror = () => {
      // onclose 会紧随其后，重连逻辑在那里处理。
    }
  }

  private cleanupSocket(): void {
    if (this.pingTimer !== null) {
      clearInterval(this.pingTimer)
      this.pingTimer = null
    }
    this.ws = null
  }

  private scheduleReconnect(): void {
    if (this.closed) return
    if (this.reconnectTimer !== null) return

    // 指数退避 + 抖动。抖动是为了避免多个客户端同时重连。
    const jitter = Math.random() * this.backoff * 0.3
    const delay = Math.min(this.backoff + jitter, MAX_BACKOFF_MS)
    this.backoff = Math.min(this.backoff * 2, MAX_BACKOFF_MS)

    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, delay)
  }

  close(): void {
    this.closed = true
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }
    this.cleanupSocket()
    if (this.ws !== null) {
      this.ws.close()
      this.ws = null
    }
  }
}
