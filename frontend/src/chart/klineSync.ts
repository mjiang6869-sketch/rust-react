// 原生 K 线快照同步。REST 只初始化/补洞；正常推送不触发 REST。
// 独立于 React，便于用假时钟覆盖慢响应、重连与限流。
import type { KlineFrame, KlinesResponse, RawCandle, StreamCandle } from '../api/types'

const HISTORY_TIMEOUT_MS = 15_000

export interface KlineSnapshot {
  bars: RawCandle[]
  source: string | null
  loading: boolean
  error: string | null
}

export interface KlineSyncOptions {
  symbol: string
  interval: string
  intervalSeconds: number
  fetch(signal: AbortSignal): Promise<KlinesResponse>
  now(): number
  cooldownMs(): number
  setTimeout(fn: () => void, ms: number): ReturnType<typeof setTimeout>
  clearTimeout(id: ReturnType<typeof setTimeout>): void
  onChange(snapshot: KlineSnapshot): void
}

export function createKlineSync(opts: KlineSyncOptions) {
  let bars: RawCandle[] = []
  let buffered = new Map<number, StreamCandle>()
  let source: string | null = null
  let historyError: string | null = null
  let streamError: string | null = 'K 线推送连接中'
  let loaded = false
  let stopped = false
  let inFlight = false
  let timer: ReturnType<typeof setTimeout> | null = null
  let abort: AbortController | null = null
  let requestTimer: ReturnType<typeof setTimeout> | null = null
  let backoff = 1_000
  let generation: string | null = null
  let disconnected = false
  let recovering = false
  let revision = 0
  let needsHistory = true

  const emit = () => {
    if (!stopped) opts.onChange({ bars, source, loading: !loaded && inFlight, error: historyError ?? streamError })
  }

  const schedule = (delay: number) => {
    if (stopped || timer !== null || inFlight || !needsHistory) return
    timer = opts.setTimeout(() => { timer = null; void load() }, delay)
  }

  const load = async () => {
    if (stopped || inFlight || !needsHistory) return
    const cooling = opts.cooldownMs()
    if (cooling > 0) {
      historyError = `K 线历史补齐等待限流冷却，约 ${Math.ceil(cooling / 1000)} 秒后重试`
      emit()
      schedule(cooling)
      return
    }
    inFlight = true
    const started = opts.now()
    const version = revision
    abort = new AbortController()
    requestTimer = opts.setTimeout(() => abort?.abort(), HISTORY_TIMEOUT_MS)
    emit()
    try {
      const response = await opts.fetch(abort.signal)
      if (stopped) return
      if (response.symbol !== opts.symbol || response.interval !== opts.interval) {
        throw new Error('K 线响应交易对或周期不符')
      }
      // 重连发生在慢请求期间：旧响应不能覆盖新的会话，串行再补一次。
      if (version !== revision) return
      const merged = new Map(response.candles.map((bar) => [bar.time, bar]))
      for (const update of buffered.values()) {
        const existing = merged.get(update.time)
        // 不让晚到的 REST 覆盖请求期间收到的 WS；已收盘帧也永不退回未收盘。
        if (!existing || (!existing.closed && (update.closed || Number(update.event_ms) >= started))) {
          merged.set(update.time, update)
        }
      }
      bars = [...merged.values()].sort((a, b) => a.time - b.time).slice(-500)
      source = '币安行情 · 历史 REST / 实时 WS（非本地归档）'
      loaded = true
      needsHistory = false
      historyError = null
      if (hasGap()) {
        needsHistory = true
        historyError = 'K 线历史仍有缺口，正在补齐'
      } else {
        backoff = 1_000
      }
    } catch (error) {
      if (stopped) return
      historyError = `K 线历史加载失败：${error instanceof Error ? error.message : String(error)}`
      needsHistory = true
    } finally {
      if (requestTimer !== null) opts.clearTimeout(requestTimer)
      requestTimer = null
      inFlight = false
      abort = null
      emit()
      if (needsHistory && !stopped) {
        schedule(Math.max(backoff, opts.cooldownMs()))
        backoff = Math.min(backoff * 2, 60_000)
      }
    }
  }

  const hasGap = () => bars.some((bar, index) => {
    const previous = bars[index - 1]
    return previous !== undefined &&
      (bar.time - previous.time !== opts.intervalSeconds || !previous.closed)
  })

  return {
    // 零延迟排程让 React StrictMode 的试挂载可以在真正请求前取消。
    start() { schedule(0) },
    frame(frame: KlineFrame | null) {
      if (stopped || frame === null || frame.interval !== opts.interval) return
      if (!frame.live) {
        recovering = generation !== null
        streamError = frame.notice ?? 'K 线推送未就绪'
        emit()
        return
      }
      if (disconnected || recovering || (generation !== null && generation !== frame.generation)) {
        revision++
        needsHistory = true
        buffered = new Map()
      }
      disconnected = false
      recovering = false
      generation = frame.generation
      streamError = null
      let changed = false
      const merged = new Map(bars.map((bar) => [bar.time, bar]))
      for (const update of frame.candles) {
        const previous = buffered.get(update.time)
        if (previous && (
          previous.closed || Number(previous.event_ms) > Number(update.event_ms) ||
          (previous.event_ms === update.event_ms && previous.close === update.close &&
            previous.open === update.open && previous.high === update.high &&
            previous.low === update.low && previous.volume === update.volume &&
            previous.closed === update.closed)
        )) continue
        buffered.set(update.time, update)
        // 缓存帧只补当前/未来的 K 线；历史已收盘的数据不被旧快照覆盖。
        const existing = merged.get(update.time)
        if (existing?.closed) continue
        merged.set(update.time, update)
        changed = true
      }
      // 限制缓存长度；不使用成交列表估算 OHLC 或成交量。
      const newest = [...buffered.keys()].sort((a, b) => b - a)
      for (const time of newest.slice(32)) buffered.delete(time)
      if (changed) bars = [...merged.values()].sort((a, b) => a.time - b.time).slice(-500)
      if (loaded && hasGap()) needsHistory = true
      emit()
      if (needsHistory) schedule(0)
    },
    disconnect() {
      if (stopped) return
      disconnected = true
      streamError = 'K 线推送断开，当前图表可能已过期；重连后自动补齐'
      emit()
    },
    stop() {
      stopped = true
      if (timer !== null) opts.clearTimeout(timer)
      if (requestTimer !== null) opts.clearTimeout(requestTimer)
      abort?.abort()
    },
  }
}
