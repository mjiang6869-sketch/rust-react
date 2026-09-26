// K 线加载。
//
// # 精度：字符串 → number 只在这一层发生
//
// 后端传的价格是字符串（`"2684.75"`），因为 bp 级数值经不起 `number` 的
// 精度损失。但 `lightweight-charts` 的 API 要求 `number`。
//
// 所以转换集中在这里，且转换后的值**不参与任何算术**——只被图表读取与绘制。
// 界面上任何显示给用户的数字（持仓价、止盈价、盈亏）仍然走字符串与
// `decimal.ts` 的定点算术。这是不把精度问题扩散出去的关键。
//
// # 为什么 lastCandle 单独暴露
//
// 最后一根可能是**未收盘**的（`closed: false`），它每时每刻都在变。界面需要
// 把它画成动态的，也需要让用户知道"这一根还没定型"。把它和已收盘的混在
// 一起，用户会以为那根已经确认。

import { useEffect, useRef, useState } from 'react'

import { api } from '../api/client'
import { cooldownRemainingMs } from '../api/cooldown'
import type { CandleBar, KlineFrame, RawCandle } from '../api/types'
import { createKlineSync, type KlineSnapshot } from './klineSync'

const INTERVAL_SECONDS: Record<string, number> = {
  '1m': 60, '3m': 180, '5m': 300, '15m': 900,
  '30m': 1800, '1h': 3600, '4h': 14400, '1d': 86400,
}

export interface KlineState {
  candles: CandleBar[]
  lastCandle: CandleBar | null
  source: string | null
  loading: boolean
  error: string | null
}

/** 历史一次 REST，当前 K 线复用行情 WebSocket。 */
export function useKlines(symbol: string, interval: string, frame: KlineFrame | null, disconnected: boolean): KlineState {
  const key = `${symbol}:${interval}`
  const empty: KlineSnapshot = { bars: [], source: null, loading: true, error: null }
  const [current, setCurrent] = useState({ key, snapshot: empty })
  // effect 清理前也不能把旧周期的价格挂在新周期标题下。
  const snapshot = current.key === key ? current.snapshot : empty
  const syncRef = useRef<ReturnType<typeof createKlineSync> | null>(null)

  useEffect(() => {
    setCurrent({ key, snapshot: { bars: [], source: null, loading: true, error: null } })
    const sync = createKlineSync({
      symbol, interval,
      intervalSeconds: INTERVAL_SECONDS[interval] ?? 900,
      // 499 根的请求权重低于 500 根；实时新增会保留最近 500 根。
      fetch: (signal) => api.klines({ symbol, interval, limit: 499, signal }),
      now: () => Date.now(),
      cooldownMs: cooldownRemainingMs,
      setTimeout: (fn, ms) => setTimeout(fn, ms),
      clearTimeout: (id) => clearTimeout(id),
      onChange: (snapshot) => setCurrent({ key, snapshot }),
    })
    syncRef.current = sync
    sync.start()
    return () => { sync.stop(); syncRef.current = null }
  }, [symbol, interval, key])

  useEffect(() => { syncRef.current?.frame(frame) }, [frame, symbol, interval])
  useEffect(() => { if (disconnected) syncRef.current?.disconnect() }, [disconnected])

  const mapped = snapshot.bars.map(toBar)
  const last = mapped.at(-1)
  const forming = last !== undefined && !last.closed
  return {
    candles: forming ? mapped.slice(0, -1) : mapped,
    lastCandle: forming ? last : null,
    source: snapshot.source,
    loading: snapshot.loading,
    error: snapshot.error,
  }
}

/** 转换只用于画图，不参与交易价格计算或文字显示。 */
function toBar(c: RawCandle): CandleBar {
  return {
    time: c.time,
    open: Number(c.open), high: Number(c.high), low: Number(c.low),
    close: Number(c.close), volume: Number(c.volume), closed: c.closed,
  }
}
