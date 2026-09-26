// 图表宿主。**这是唯一接触 lightweight-charts 的 React 文件。**
//
// 历史加载/补齐用 setData；实时推送只更新末根或追加新根。
// 指纹覆盖完整 OHLCV，收盘价不变但成交量变化时也必须更新。
//
// # 为什么缩放位置必须保住
//
// 用户手动缩放到某段区间后，如果每次数据刷新都 `fitContent`，视图会跳回
// 全览——这是行情软件最让人恼火的体验之一。所以只在**首次加载**和**换周期**
// 时显示最近 100 根，后续刷新保持用户当前的缩放。

import { useCallback, useEffect, useRef, useState } from 'react'
import {
  CandlestickSeries,
  CrosshairMode,
  HistogramSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type MouseEventParams,
  type UTCTimestamp,
  TickMarkType,
  type Time,
} from 'lightweight-charts'

import { Copy } from 'lucide-react'
import { Input } from '../components/FormControls'
import { ContextMenu } from '../components/ContextMenu'
import type { CandleBar, RawCandle } from '../api/types'
import { signedStr } from '../api/decimal'
import { pnlClass } from '../format'
import { OrderLines } from './orderLines'
import { chartUpdatePlan } from './chartUpdate'

export interface ChartHostProps {
  /** 悬停展示保留原始字符串，不从图表浮点数反向生成价格。 */
  rawCandles: RawCandle[]
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
  rawCandles,
  candles,
  lastCandle,
  levels,
  intervalKey,
  height = 460,
  historyReady = true,
}: ChartHostProps) {
  const [hovered, setHovered] = useState<{ key: string; time: number } | null>(null)
  const activeKeyRef = useRef(intervalKey)
  useEffect(() => {
    activeKeyRef.current = intervalKey
    setHovered(null)
  }, [intervalKey])
  const selected = hovered?.key === intervalKey
    ? rawCandles.find((bar) => bar.time === hovered.time)
    : undefined
  const details = selected ?? rawCandles.at(-1)
  const containerRef = useRef<HTMLDivElement | null>(null)
  const chartRef = useRef<IChartApi | null>(null)
  const candleSeriesRef = useRef<ISeriesApi<'Candlestick'> | null>(null)
  const volumeSeriesRef = useRef<ISeriesApi<'Histogram'> | null>(null)
  const orderLinesRef = useRef<OrderLines | null>(null)

  const [menu, setMenu] = useState<{ x: number; y: number; price: string } | null>(null)
  const [copyStatus, setCopyStatus] = useState('')
  const [copyError, setCopyError] = useState(false)
  const closeMenu = useCallback(() => setMenu(null), [])
  useEffect(() => { closeMenu(); setCopyStatus('') }, [intervalKey, closeMenu])

  function openPriceMenu(clientX: number, clientY: number) {
    const container = containerRef.current
    const chart = chartRef.current
    const series = candleSeriesRef.current
    if (!container || !chart || !series || rawCandles.length === 0) return
    const bounds = container.getBoundingClientRect()
    const x = clientX - bounds.left
    const y = clientY - bounds.top
    const pane = chart.paneSize()
    if (x < 0 || y < 0 || x >= pane.width || y >= pane.height) return
    // 这是光标在价格轴上的坐标，不用于下单或保护单计算。
    const coordinatePrice = series.coordinateToPrice(y)
    if (coordinatePrice === null || !Number.isFinite(coordinatePrice)) return
    const price = series.priceFormatter().format(coordinatePrice)
    container.focus({ preventScroll: true })
    setCopyStatus('')
    setCopyError(false)
    setMenu({ x: clientX, y: clientY, price })
  }

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
      bg: style.getPropertyValue('--chart-bg').trim(),
      text: style.getPropertyValue('--chart-text').trim(),
      grid: style.getPropertyValue('--chart-grid').trim(),
      border: style.getPropertyValue('--border').trim(),
      up: style.getPropertyValue('--pos').trim(),
      down: style.getPropertyValue('--neg').trim(),
      label: style.getPropertyValue('--chart-label').trim(),
      crosshair: style.getPropertyValue('--chart-crosshair').trim(),
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
        vertLines: { color: theme.grid, visible: false },
        horzLines: { color: theme.grid },
      },
      crosshair: {
        mode: CrosshairMode.Normal,
        vertLine: { color: theme.crosshair, labelBackgroundColor: theme.label },
        horzLine: { color: theme.crosshair, labelBackgroundColor: theme.label },
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
      // 实体与影线保持同色，移除多余描边。
      borderVisible: false,
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

    const onCrosshairMove = (event: MouseEventParams) => {
      const point = event.point
      const time = event.time
      if (!point || point.x < 0 || point.y < 0 ||
          point.x >= container.clientWidth || point.y >= container.clientHeight ||
          typeof time !== 'number' || !event.seriesData.has(candlesSeries)) {
        setHovered(null)
        return
      }
      setHovered((current) => current?.key === activeKeyRef.current && current.time === time
        ? current : { key: activeKeyRef.current, time })
    }
    chart.subscribeCrosshairMove(onCrosshairMove)

    // 容器尺寸变化时重算宽度——不做的话窗口缩放会让图表留白。
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0]
      if (entry !== undefined) {
        chart.applyOptions({ width: Math.floor(entry.contentRect.width), height: Math.floor(entry.contentRect.height) })
      }
    })
    observer.observe(container)

    return () => {
      chart.unsubscribeCrosshairMove(onCrosshairMove)
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
      color: b.close >= b.open ? `${up}55` : `${down}55`,
    })
    // 通常只变末根（或收盘后追加一根），走增量更新；补历史才整段替换。
    if (plan.kind === 'update') {
      for (const bar of all.slice(plan.from)) {
        series.update(pricePoint(bar))
        volume.update(volumePoint(bar))
      }
    } else if (plan.kind !== 'skip') {
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
        // 仅设置可见窗口；已加载的历史仍然可以向左拖动查看。
        chartRef.current?.timeScale().setVisibleLogicalRange({
          from: Math.max(0, all.length - 100) - 1,
          to: all.length - 1 + 4,
        })
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
    <div className="chart-host" style={{ height }}>
      <div className="chart-details" aria-label="K 线数据">
        <div className="chart-details-heading">
          <span>{selected ? '所选 K 线' : '最新 K 线'}</span>
          <time>{details ? chartTime(details.time as UTCTimestamp, {
            year: 'numeric', month: '2-digit', day: '2-digit',
            hour: '2-digit', minute: '2-digit', hour12: false,
          }) : '—'} · UTC+8</time>
          {details && <span>{details.closed ? '已收盘' : '未收盘'}</span>}
        </div>
        <dl className="chart-details-values">
          {([
            ['开', 'open'], ['高', 'high'], ['低', 'low'], ['收', 'close'], ['量', 'volume'],
          ] as const).map(([label, field]) => (
            <div key={field} className={`candle-${field}`}><dt>{label}</dt><dd>{details?.[field] ?? '—'}</dd></div>
          ))}
          <div className={pnlClass(details?.change ?? null)} title="收盘价 − 开盘价">
            <dt>涨跌额</dt><dd>{details?.change == null ? '—' : signedStr(details.change)}</dd>
          </div>
          <div className={pnlClass(details?.change ?? null)} title="（收盘价 − 开盘价）÷ 开盘价 × 100%">
            <dt>涨跌幅</dt><dd>{details?.change_percent == null ? '—' : `${signedStr(details.change_percent, 4)}%`}</dd>
          </div>
        </dl>
      </div>
      <div
        ref={containerRef}
        className="chart-plot"
        tabIndex={0}
        onContextMenu={(event) => {
          event.preventDefault()
          openPriceMenu(event.clientX, event.clientY)
        }}
        onKeyDown={(event) => {
          if (event.key !== 'ContextMenu' && !(event.shiftKey && event.key === 'F10')) return
          event.preventDefault()
          const bounds = containerRef.current?.getBoundingClientRect()
          const pane = chartRef.current?.paneSize()
          if (bounds && pane) openPriceMenu(bounds.left + pane.width / 2, bounds.top + pane.height / 2)
        }}
        role="img"
        aria-label="价格走势图，绿涨红跌，含 K 线、成交量与持仓价位线"
      />
      {copyStatus && <div className="chart-copy-status" role="status">{copyStatus}</div>}
      {menu && <ContextMenu x={menu.x} y={menu.y} label="图表价格操作" onClose={closeMenu}>
        <div className="context-menu-caption">光标价格</div>
        <button type="button" role="menuitem" className="context-menu-item" onClick={() => {
          if (!navigator.clipboard) { setCopyError(true); return }
          void navigator.clipboard.writeText(menu.price).then(() => {
            setCopyStatus(`已复制价格 ${menu.price}`)
            closeMenu()
            containerRef.current?.focus({ preventScroll: true })
          }).catch(() => setCopyError(true))
        }}><Copy size={16} aria-hidden="true" /><span>复制价格</span><strong>{menu.price}</strong></button>
        {copyError && <div className="context-copy-fallback">
          <p role="alert">复制失败，请选中价格手动复制。</p>
          <Input aria-label="待复制价格" readOnly value={menu.price} onFocus={(event) => event.target.select()} />
        </div>}
      </ContextMenu>}
    </div>
  )
}

/** 只格式化坐标标签，原始 UTC 时间戳不变。 */
function chartTime(value: Time, options: Intl.DateTimeFormatOptions): string {
  const date = typeof value === 'number' ? new Date(value * 1000)
    : typeof value === 'string' ? new Date(value)
    : new Date(Date.UTC(value.year, value.month - 1, value.day))
  return date.toLocaleString('zh-CN', { ...options, timeZone: 'Asia/Shanghai' })
}
