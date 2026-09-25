// 图表宿主。**这是唯一接触 lightweight-charts 的 React 文件。**
//
// # 性能铁律
//
// K 线数据**不经过 React state**。每根 K 线更新都触发 React 重渲染会让整棵树
// 重新协调——在 1 分钟 K 线加上实时推送的场景下这是最大的性能陷阱。
//
// 正确做法是数据直接从 WebSocket 处理器进 series（`series.update()`），
// React 只负责创建与销毁图表实例。
//
// # 为什么把图表放在独立组件而不是内联在 App 里
//
// lightweight-charts 是命令式 API（创建实例、附加 series、订阅事件），而
// React 是声明式的。混在一起最容易出的错是「effect 依赖变化导致图表被反复
// 重建」——图表每次重建都会丢失缩放位置与所有标注。这里用 ref 持有实例，
// 且 effect 的依赖列表是空的（只在挂载时创建一次）。

import { useEffect, useRef } from 'react'
import {
  CandlestickSeries,
  HistogramSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from 'lightweight-charts'

import type { CandleBar } from '../api/types'
import { OrderLines } from './orderLines'

export interface ChartHostProps {
  /** 初始 K 线。后续更新通过 `seriesRef` 直接推入，不走 React。 */
  initialCandles: CandleBar[]
  /** 持仓与订单的价位线。 */
  levels: { entry: string | null; stop: string | null; takeProfits: string[] }
  /** 供父组件拿到 series，用于直接推送更新。 */
  onReady?: (handle: ChartHandle) => void
  height?: number
}

/** 供外部直接操作图表（绕过 React）。 */
export interface ChartHandle {
  updateCandle: (bar: CandleBar) => void
  setCandles: (bars: CandleBar[]) => void
  fitContent: () => void
}

export function ChartHost({
  initialCandles,
  levels,
  onReady,
  height = 420,
}: ChartHostProps) {
  const containerRef = useRef<HTMLDivElement | null>(null)
  const chartRef = useRef<IChartApi | null>(null)
  const candleSeriesRef = useRef<ISeriesApi<'Candlestick'> | null>(null)
  const volumeSeriesRef = useRef<ISeriesApi<'Histogram'> | null>(null)
  const orderLinesRef = useRef<OrderLines | null>(null)

  // 创建图表。依赖列表刻意为空——只在挂载时创建一次。
  useEffect(() => {
    const container = containerRef.current
    if (container === null) return

    const chart = createChart(container, {
      layout: {
        background: { color: '#0f1115' },
        textColor: '#c9d1d9',
        attributionLogo: false,
      },
      grid: {
        vertLines: { color: '#1c2029' },
        horzLines: { color: '#1c2029' },
      },
      crosshair: { mode: 1 },
      rightPriceScale: { borderColor: '#2a3038' },
      timeScale: {
        borderColor: '#2a3038',
        timeVisible: true,
        secondsVisible: false,
      },
      localization: {
        // 中文界面：时间轴与价格轴都用本地格式
        locale: 'zh-CN',
      },
    })

    // v5 用 addSeries(CandlestickSeries) 而非 addCandlestickSeries()
    const candles = chart.addSeries(CandlestickSeries, {
      upColor: '#26a69a',
      downColor: '#ef5350',
      borderUpColor: '#26a69a',
      borderDownColor: '#ef5350',
      wickUpColor: '#26a69a',
      wickDownColor: '#ef5350',
    })

    const volume = chart.addSeries(HistogramSeries, {
      priceFormat: { type: 'volume' },
      priceScaleId: 'volume',
    })
    // 成交量单独一栏，避免压扁价格轴
    chart.priceScale('volume').applyOptions({
      scaleMargins: { top: 0.8, bottom: 0 },
    })

    chartRef.current = chart
    candleSeriesRef.current = candles
    volumeSeriesRef.current = volume

    const handle: ChartHandle = {
      updateCandle: (bar) => {
        candles.update({
          time: bar.time as UTCTimestamp,
          open: bar.open,
          high: bar.high,
          low: bar.low,
          close: bar.close,
        })
        volume.update({
          time: bar.time as UTCTimestamp,
          value: bar.volume,
          color: bar.close >= bar.open ? '#26a69a55' : '#ef535055',
        })
      },
      setCandles: (bars) => {
        candles.setData(
          bars.map((b) => ({
            time: b.time as UTCTimestamp,
            open: b.open,
            high: b.high,
            low: b.low,
            close: b.close,
          })),
        )
        volume.setData(
          bars.map((b) => ({
            time: b.time as UTCTimestamp,
            value: b.volume,
            color: b.close >= b.open ? '#26a69a55' : '#ef535055',
          })),
        )
      },
      fitContent: () => chart.timeScale().fitContent(),
    }
    onReady?.(handle)

    // 容器尺寸变化时重算宽度——不做的话窗口缩放会让图表留白。
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0]
      if (entry !== undefined) {
        chart.applyOptions({ width: entry.contentRect.width })
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
  }, [onReady])

  // 初始数据。只在挂载时设置一次；后续更新走 handle。
  useEffect(() => {
    if (initialCandles.length === 0) return
    const candles = candleSeriesRef.current
    const volume = volumeSeriesRef.current
    if (candles === null || volume === null) return

    candles.setData(
      initialCandles.map((b) => ({
        time: b.time as UTCTimestamp,
        open: b.open,
        high: b.high,
        low: b.low,
        close: b.close,
      })),
    )
    volume.setData(
      initialCandles.map((b) => ({
        time: b.time as UTCTimestamp,
        value: b.volume,
        color: b.close >= b.open ? '#26a69a55' : '#ef535055',
      })),
    )
    chartRef.current?.timeScale().fitContent()
    // 刻意只在挂载时执行——后续数据更新通过 handle 直接推入。
    // eslint 之外的说明：initialCandles 变化是父组件重新渲染导致的，
    // 这里不希望重置用户的缩放位置。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // 价位线。这些可以走 React，因为它们的更新频率远低于 K 线。
  useEffect(() => {
    const candles = candleSeriesRef.current
    if (candles === null) return

    if (orderLinesRef.current === null) {
      orderLinesRef.current = new OrderLines(candles)
    }
    orderLinesRef.current.apply(levels)
  }, [levels])

  return (
    <div
      ref={containerRef}
      style={{ width: '100%', height }}
      role="img"
      aria-label="价格图表，显示 K 线与持仓价位"
    />
  )
}
