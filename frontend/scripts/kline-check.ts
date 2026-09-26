import { chartUpdatePlan } from '../src/chart/chartUpdate.ts'
import type { CandleBar } from '../src/api/types.ts'
// node --experimental-strip-types scripts/kline-check.ts
import { createKlineSync, type KlineSnapshot } from '../src/chart/klineSync.ts'
import type { KlineFrame, KlinesResponse, StreamCandle } from '../src/api/types.ts'

function assert(value: unknown, message: string): void {
  if (!value) throw new Error(message)
}
const candle = (time: number, close = '101', closed = false, event = 1_000_001): StreamCandle => ({
  time, open: '100', high: '105', low: '99', close, volume: '12', closed, event_ms: String(event),
})
const frame = (candles: StreamCandle[], generation = '1'): KlineFrame => ({
  interval: '1m', live: true, notice: null, generation, candles,
})
const response = (candles: StreamCandle[]): KlinesResponse => ({
  symbol: 'ETHUSDC', interval: '1m', source: 'test', candles,
})
function harness() {
  let now = 1_000_000
  let seq = 0
  let cooling = 0
  let requests = 0
  let peak = 0
  let running = 0
  let snapshot: KlineSnapshot = { bars: [], source: null, error: null, loading: false }
  const timers = new Map<number, { at: number; fn: () => void }>()
  const pending: { resolve: (r: KlinesResponse) => void; reject: (e: Error) => void; signal: AbortSignal }[] = []
  const sync = createKlineSync({
    symbol: 'ETHUSDC', interval: '1m', intervalSeconds: 60,
    now: () => now, cooldownMs: () => Math.max(0, cooling - now),
    setTimeout: (fn, ms) => { const id = ++seq; timers.set(id, { at: now + ms, fn }); return id as unknown as ReturnType<typeof setTimeout> },
    clearTimeout: (id) => { timers.delete(id as unknown as number) },
    fetch: (signal) => {
      requests++; running++; peak = Math.max(peak, running)
      return new Promise<KlinesResponse>((resolve, reject) => {
        let settled = false
        const finish = () => {
          if (settled) return false
          settled = true
          running--
          return true
        }
        const fail = (error: Error) => { if (finish()) reject(error) }
        signal.addEventListener('abort', () => fail(new Error('请求已取消')), { once: true })
        pending.push({ resolve: (r) => { if (finish()) resolve(r) }, reject: fail, signal })
      })
    },
    onChange: (value) => { snapshot = value },
  })
  return {
    sync, pending,
    get requests() { return requests }, get peak() { return peak }, get snapshot() { return snapshot },
    cool(ms: number) { cooling = now + ms },
    async advance(ms: number) {
      const target = now + ms
      for (;;) {
        const next = [...timers.entries()].filter(([, t]) => t.at <= target).sort((a, b) => a[1].at - b[1].at)[0]
        if (!next) break
        now = next[1].at; timers.delete(next[0]); next[1].fn()
        await Promise.resolve(); await Promise.resolve()
      }
      now = target
      await Promise.resolve(); await Promise.resolve()
    },
  }
}

// 慢历史响应不得回滚推送；正常更新、收盘换根、重复帧不请求历史。
{
  const h = harness(); h.sync.start(); await h.advance(0)
  h.sync.frame(frame([candle(960, '103')]))
  h.pending.shift()?.resolve(response([candle(900, '100', true), candle(960, '101')]))
  await h.advance(0)
  assert(h.snapshot.bars.at(-1)?.close === '103', '晚到 REST 回滚了 WS')
  h.sync.frame(frame([candle(960, '103', true, 1_000_001)]))
  assert(h.snapshot.bars.at(-1)?.closed, '同毫秒收盘帧不能被去重')
  h.sync.frame(frame([candle(960, '104', true, 1_020_000), candle(1020, '104', false, 1_020_001)]))
  for (let i = 0; i < 100; i++) h.sync.frame(frame([candle(960, '104', true, 1_020_000), candle(1020, '104', false, 1_020_001)]))
  await h.advance(600_000)
  assert(h.requests === 1, '正常推送不应产生轮询 REST')
  assert(h.snapshot.bars.length === 3 && h.snapshot.bars[1]?.closed, '跨周期丢了收盘帧')
  h.sync.frame(frame([candle(1020, '80', false, 1_020_000)]))
  assert(h.snapshot.bars.at(-1)?.close === '104', '乱序帧回滚价格')
  console.log('PASS: 慢响应、收盘换根、重复/乱序帧；10 分钟仅初始化 1 次 REST')
  h.sync.stop()
}
// 缺口触发一次补齐，多个帧不并发请求；慢补齐期间的 WS 必须保留。
{
  const h = harness(); h.sync.start(); await h.advance(0)
  h.pending.shift()?.resolve(response([candle(900, '100', true), candle(960)])); await h.advance(0)
  h.sync.frame(frame([candle(1140, '105', false, 1_140_001)])); await h.advance(0)
  h.sync.frame(frame([candle(1140, '106', false, 1_140_002)])); await h.advance(0)
  assert(h.requests === 2 && h.peak === 1, '补洞请求应合并且串行')
  h.pending.shift()?.resolve(response([candle(900, '100', true), candle(960, '101', true), candle(1020, '102', true), candle(1080, '103', true), candle(1140, '104')]))
  await h.advance(0)
  assert(h.snapshot.bars.at(-1)?.close === '106', '补洞回滚了实时价格')
  await h.advance(60_000); assert(h.requests === 2, '补齐后应停止请求')
  console.log('PASS: 缺口补齐串行合并，实时帧覆盖慢历史响应')
  h.sync.stop()
}
// 浏览器断线和上游 session 变化均补一次；断线状态本身不循环请求。
{
  const h = harness(); h.sync.start(); await h.advance(0)
  h.sync.frame(frame([candle(960)]))
  h.pending.shift()?.resolve(response([candle(960)])); await h.advance(0)
  h.sync.disconnect(); await h.advance(30_000)
  assert(h.requests === 1 && h.snapshot.error?.includes('断开'), '断线应告警且不轮询')
  h.sync.frame(frame([candle(960)])); await h.advance(0)
  assert(h.requests === 2, '浏览器恢复要补历史')
  h.pending.shift()?.resolve(response([candle(960)])); await h.advance(0)
  h.sync.frame(frame([candle(960)], '2')); await h.advance(0)
  assert(h.requests === 3, '上游短断线即使未显示也须补历史')
  assert(h.peak === 1, '重连请求不应并发')
  h.sync.stop(); assert(h.pending[0]?.signal.aborted, '卸载应取消请求')
  console.log('PASS: 浏览器/上游重连补齐，卸载取消请求')
}
// 限流和失败只在退避到期后重试，切换后旧响应不写状态。
{
  const h = harness(); h.cool(5_000); h.sync.start(); await h.advance(4_999)
  assert(h.requests === 0, '冷却期不能发 REST')
  await h.advance(1); assert(h.requests === 1, '冷却到期重试一次')
  h.pending.shift()?.reject(new Error('临时错误')); await h.advance(0); await h.advance(999)
  assert(h.requests === 1, '错误重试必须退避')
  await h.advance(1); assert(h.requests === 2, '退避到期重试')
  h.sync.stop(); const before = h.snapshot
  h.pending.shift()?.resolve(response([candle(960)])); await h.advance(100_000)
  assert(h.snapshot === before && h.requests === 2, '卸载后旧请求不得写状态或重试')
  console.log('PASS: 限流、错误退避、卸载后迟到响应失效')
}
// StrictMode 试挂载与错误周期帧不会多发初始化请求。
{
  const h = harness(); h.sync.start(); h.sync.stop(); await h.advance(0)
  assert(h.requests === 0, '试挂载不应打 REST')
  const other = harness(); other.sync.frame({ ...frame([candle(960)]), interval: '15m' })
  assert(other.snapshot.bars.length === 0, '错误周期帧被接受')
  other.sync.stop()
  console.log('PASS: StrictMode 去重，旧周期隔离')
}

// 慢 REST 期间换上游会话，旧会话快照无效且新请求必须串行。
{
  const h = harness(); h.sync.start(); await h.advance(0)
  h.sync.frame(frame([candle(960, '103')]))
  h.sync.frame(frame([candle(1020, '104', false, 1_020_001)], '2'))
  h.pending.shift()?.resolve(response([candle(960, '80')]))
  await h.advance(0)
  assert(h.snapshot.bars.at(-1)?.close === '104', '旧会话 REST 覆盖了新推送')
  await h.advance(1000)
  assert(h.requests === 2 && h.peak === 1, '会话变化需要串行补历史')
  h.sync.stop()
  console.log('PASS: 慢 REST 期间换会话，旧响应隔离且串行补齐')
}

// 图表层真实分支：WS 先到、历史后到必须整体替换，禁止向过去 update。
{
  const bar = (time: number, volume = 12): CandleBar => ({ time, open: 100, high: 103, low: 99, close: 101, volume, closed: false })
  assert(chartUpdatePlan([bar(960)], [bar(900), bar(960)], true).kind === 'replace', '补入历史不能走增量')
  assert(chartUpdatePlan([bar(900), bar(960)], [bar(900), bar(960, 13)], true).kind === 'update', '价格不变、成交量变化必须更新')
  assert(chartUpdatePlan([bar(900), bar(960)], [bar(900), bar(960), bar(1020)], true).kind === 'update', '新 K 线应增量追加')
  assert(chartUpdatePlan([bar(900), bar(960)], [bar(960), bar(1020)], true).kind === 'replace', '窗口滚动裁剪必须替换')
  assert(chartUpdatePlan([bar(900)], [bar(900)], true).kind === 'skip', '重复帧应跳过')
  assert(chartUpdatePlan([bar(900)], [bar(900)], false).kind === 'replace', '周期变化必须替换')
  console.log('PASS: 图表历史补入、末根成交量更新、追加、裁剪与周期切换')
}

// 上游一直返回有缺口的历史，退避仍须增长，不能退化为每秒请求。
{
  const h = harness(); h.sync.start(); await h.advance(0)
  const incomplete = response([candle(900, '100', true), candle(1020)])
  h.pending.shift()?.resolve(incomplete); await h.advance(0)
  await h.advance(1000)
  assert(h.requests === 2, '第一次补洞应在 1 秒后')
  h.pending.shift()?.resolve(incomplete); await h.advance(0)
  await h.advance(1000)
  assert(h.requests === 2, '缺口未解决时不能复位退避')
  await h.advance(1000)
  assert(h.requests === 3 && h.peak === 1, '第二次补洞应等待 2 秒')
  h.sync.stop()
  console.log('PASS: 持续缺口保持指数退避')
}

// 历史请求无响应时取消并退避重试，不允许永久卡住或重叠。
{
  const h = harness(); h.sync.start(); await h.advance(15_000)
  assert(h.pending.shift()?.signal.aborted, '请求超时必须取消')
  assert(h.snapshot.error?.includes('历史加载失败'), '超时必须显示错误')
  await h.advance(1000)
  assert(h.requests === 2 && h.peak === 1, '超时后串行重试')
  h.sync.stop()
  console.log('PASS: 历史请求超时取消并串行重试')
}
