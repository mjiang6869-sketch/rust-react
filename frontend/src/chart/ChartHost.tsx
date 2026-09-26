// 图表宿主。**这是唯一接触 lightweight-charts 的 React 文件。**
//
// 历史加载/补齐用 setData；实时推送只更新末根或追加新根。
// 指纹覆盖完整 OHLCV，收盘价不变但成交量变化时也必须更新。
//
// # 为什么缩放位置必须保住
//
// 用户手动缩放到某段区间后，如果每次数据刷新都 `fitContent`，视图会跳回
// 全览——这是行情软件最让人恼火的体验之一。所以只在**首次加载**和**换周期**
// 时自适应，后续刷新保持用户当前的缩放。

import { useEffect, useRef } from 'react'
import {
  CandlestickSeries,
  HistogramSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
  TickMarkType,
  type Time,
} from 'lightweight-charts'

import type { CandleBar } from '../api/types'
import { OrderLines } from './orderLines'
import { chartUpdatePlan } from './chartUpdate'

export interface ChartHostProps {
  /** 已收盘的 K 线。 */
  candles: CandleBar[]
  /** 当前正在形成的一根（`closed: false`）。画成动态的。 */
  lastCandle: CandleBar | null
  /** 持仓与订单的价位线。 */
  levels: { entry: string | null; stop: string | null; takeProfits: string[] }
  /** 周期标识。变化时重置缩放位置。 */
  intervalKey: string
  historyReady?: boolean
  height?: number | string
}

export function ChartHost({
  candles,
  lastCandle,
  levels,
  intervalKey,
  height = 460,
  historyReady = true,
}: ChartHostProps) {
  const containerRef = useRef<HTMLDivElement | null>(null)
  const chartRef = useRef<IChartApi | null>(null)
  const candleSeriesRef = useRef<ISeriesApi<'Candlestick'> | null>(null)
  const volumeSeriesRef = useRef<ISeriesApi<'Histogram'> | null>(null)
  const orderLinesRef = useRef<OrderLines | null>(null)

  // 保留上一次数据，判断推送可否增量更新。
  const previousBarsRef = useRef<CandleBar[]>([])
  // 上一次的周期，用于判断是否要重新自适应
  const lastIntervalRef = useRef<string>('')

  // --- 创建图表。依赖列表刻意为空：只在挂载时创建一次。 ---
  useEffect(() => {
    const container = containerRef.current
    if (container === null) return

    // Canvas 读取同一套语义色，避免 K 线与盘口的涨跌颜色不一致。
    const style = getComputedStyle(container)
    const theme = {
      bg: style.getPropertyValue('--bg-panel').trim(),
      text: style.getPropertyValue('--text-dim').trim(),
      grid: style.getPropertyValue('--border').trim(),
      border: style.getPropertyValue('--border').trim(),
      up: style.getPropertyValue('--pos').trim(),
      down: style.getPropertyValue('--neg').trim(),
      crosshair: style.getPropertyValue('--text-dim').trim(),
    }
    const chart = createChart(container, {
      layout: {
        background: { color: theme.bg },
        textColor: theme.text,
        attributionLogo: false,
        fontFamily:
          "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, monospace",
        fontSize: 12,
      },
      grid: {
        vertLines: { color: theme.grid },
        horzLines: { color: theme.grid },
      },
      crosshair: {
        mode: 1,
        vertLine: { color: theme.crosshair, labelBackgroundColor: '#2b323d' },
        horzLine: { color: theme.crosshair, labelBackgroundColor: '#2b323d' },
      },
      rightPriceScale: {
        borderColor: theme.border,
        // 做市看的是 bp 级波动，价格轴需要更多刻度
        scaleMargins: { top: 0.08, bottom: 0.22 },
      },
      timeScale: {
        borderColor: theme.border,
        timeVisible: true,
        secondsVisible: false,
        rightOffset: 4,
        tickMarkFormatter: (time: Time, type: TickMarkType) => {
          const options: Intl.DateTimeFormatOptions =
            type === TickMarkType.Year ? { year: 'numeric' } :
            type === TickMarkType.Month ? { month: 'short' } :
            type === TickMarkType.DayOfMonth ? { month: '2-digit', day: '2-digit' } :
            { hour: '2-digit', minute: '2-digit', hour12: false }
          return chartTime(time, options)
        },
      },
      localization: { locale: 'zh-CN', timeFormatter: (time: Time) => chartTime(time, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false }) },
    })

    // v5 用 addSeries(CandlestickSeries) 而非 addCandlestickSeries()
    const candlesSeries = chart.addSeries(CandlestickSeries, {
      upColor: theme.up,
      downColor: theme.down,
      borderUpColor: theme.up,
      borderDownColor: theme.down,
      wickUpColor: theme.up,
      wickDownColor: theme.down,
      // 未收盘的那根用细边框区分，让人一眼看出它还没定型
      borderVisible: true,
    })

    const volume = chart.addSeries(HistogramSeries, {
      priceFormat: { type: 'volume' },
      priceScaleId: 'volume',
      lastValueVisible: false,
      priceLineVisible: false,
    })
    // 成交量单独一栏，避免压扁价格轴
    chart.priceScale('volume').applyOptions({
      scaleMargins: { top: 0.82, bottom: 0 },
    })

    chartRef.current = chart
    candleSeriesRef.current = candlesSeries
    volumeSeriesRef.current = volume

    // 容器尺寸变化时重算宽度——不做的话窗口缩放会让图表留白。
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0]
      if (entry !== undefined) {
        chart.applyOptions({ width: Math.floor(entry.contentRect.width), height: Math.floor(entry.contentRect.height) })
      }
    })
    observer.observe(container)

    return () => {
      observer.disconnect()
      chart.remove()
      chartRef.current = null
      candleSeriesRef.current = null
      volumeSeriesRef.current = null
      orderLinesRef.current = null
    }
  }, [])

  // --- 数据更新 ---
  useEffect(() => {
    const series = candleSeriesRef.current
    const volume = volumeSeriesRef.current
    if (series === null || volume === null) return

    // 已收盘 + 未收盘拼成完整序列。图表需要连续的时间轴，
    // 所以未收盘那根也必须在数据里。
    const all = lastCandle === null ? candles : [...candles, lastCandle]

    const previous = previousBarsRef.current
    const sameInterval = lastIntervalRef.current === intervalKey
    const plan = chartUpdatePlan(previous, all, sameInterval)
    if (plan.kind === 'skip') return
    const container = containerRef.current
    if (container === null) return
    const style = getComputedStyle(container)
    const up = style.getPropertyValue('--pos').trim()
    const down = style.getPropertyValue('--neg').trim()
    const pricePoint = (b: CandleBar) => ({
      time: b.time as UTCTimestamp, open: b.open, high: b.high, low: b.low, close: b.close,
    })
    const volumePoint = (b: CandleBar) => ({
      time: b.time as UTCTimestamp, value: b.volume,
      color: b.close >= b.open ? `${up}44` : `${down}44`,
    })
    // 通常只变末根（或收盘后追加一根），走增量更新；补历史才整段替换。
    if (plan.kind === 'update') {
      for (const bar of all.slice(plan.from)) {
        series.update(pricePoint(bar))
        volume.update(volumePoint(bar))
      }
    } else {
      series.setData(all.map(pricePoint))
      volume.setData(all.map(volumePoint))
    }
    previousBarsRef.current = all

    // 只在首次加载、或周期变化时自适应缩放。
    // 每次推送都 fitContent 会把用户的缩放位置冲掉。
    if (
      lastIntervalRef.current !== intervalKey ||
      lastIntervalRef.current === ''
    ) {
      if (all.length > 0 && historyReady) {
        lastIntervalRef.current = intervalKey
        chartRef.current?.timeScale().fitContent()
      }
    }
  }, [candles, lastCandle, intervalKey, historyReady])

  // --- 价位线。更新频率远低于 K 线，可以走 React。 ---
  useEffect(() => {
    const series = candleSeriesRef.current
    if (series === null) return

    if (orderLinesRef.current === null) {
      orderLinesRef.current = new OrderLines(series)
    }
    orderLinesRef.current.apply(levels)
  }, [levels])

  return (
    <div
      ref={containerRef}
      style={{ width: '100%', height }}
      role="img"
      aria-label="价格走势图，含 K 线与持仓价位线"
    />
  )
}

/** 只格式化坐标标签，原始 UTC 时间戳不变。 */
function chartTime(value: Time, options: Intl.DateTimeFormatOptions): string {
  const date = typeof value === 'number' ? new Date(value * 1000)
    : typeof value === 'string' ? new Date(value)
    : new Date(Date.UTC(value.year, value.month - 1, value.day))
  return date.toLocaleString('zh-CN', { ...options, timeZone: 'Asia/Shanghai' })
}
