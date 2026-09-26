// 资金曲线的数值转换只用于 SVG 像素定位；悬停和值标签始终用后端原始字符串。

import { useId, useMemo, useState, type MouseEvent } from 'react'

import type { OverviewData } from '../api/types'
import { num, time } from '../format'

type Point = OverviewData['curve'][number]

const WIDTH = 1000
const HEIGHT = 236
const PAD_X = 14
const PAD_Y = 24

export function EquityCurve({ points, asset, trend }: { points: Point[]; asset: string; trend: string }) {
  const gradientId = useId().replaceAll(':', '')
  const [hovered, setHovered] = useState<number | null>(null)
  const plot = useMemo(() => {
    const valid = points.map((point) => ({
      source: point,
      timestamp: Date.parse(point.at),
      value: Number(point.equity),
    })).filter((point) => Number.isFinite(point.timestamp) && Number.isFinite(point.value))
    if (valid.length < 2) return []
    const minTime = valid[0]!.timestamp
    const maxTime = valid[valid.length - 1]!.timestamp
    const values = valid.map((point) => point.value)
    const min = Math.min(...values)
    const max = Math.max(...values)
    const padding = Math.max((max - min) * 0.12, Math.abs(max) * 0.0005, 0.01)
    const range = max - min + padding * 2
    return valid.map((point) => ({
      source: point.source,
      x: PAD_X + ((point.timestamp - minTime) / Math.max(1, maxTime - minTime)) * (WIDTH - PAD_X * 2),
      y: PAD_Y + ((max + padding - point.value) / range) * (HEIGHT - PAD_Y * 2),
    }))
  }, [points])

  const first = plot[0]
  const last = plot[plot.length - 1]
  if (!first || !last || plot.length < 2) {
    return <div className="overview-curve-empty">资金曲线从本次运行开始记录，等待下一次账户快照。</div>
  }

  const selected = plot[hovered ?? plot.length - 1] ?? last
  const line = plot.map((point, index) => `${index === 0 ? 'M' : 'L'}${point.x.toFixed(2)} ${point.y.toFixed(2)}`).join(' ')
  const area = `${line} L${last.x.toFixed(2)} ${HEIGHT} L${first.x.toFixed(2)} ${HEIGHT} Z`

  function selectPoint(event: MouseEvent<SVGSVGElement>) {
    const bounds = event.currentTarget.getBoundingClientRect()
    const x = ((event.clientX - bounds.left) / bounds.width) * WIDTH
    let nearest = 0
    for (let index = 1; index < plot.length; index += 1) {
      if (Math.abs(plot[index]!.x - x) < Math.abs(plot[nearest]!.x - x)) nearest = index
    }
    setHovered(nearest)
  }

  return <div className={`overview-curve ${trend}`}>
    <div className="overview-curve-readout">
      <span>{time(selected.source.at)}</span>
      <strong>{num(selected.source.equity, 2)} <small>{asset}</small></strong>
    </div>
    <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} preserveAspectRatio="none"
      role="img" aria-label={`本次运行资金曲线，从 ${num(first.source.equity, 2)} 到 ${num(last.source.equity, 2)} ${asset}`}
      onMouseMove={selectPoint} onMouseLeave={() => setHovered(null)}>
      <defs><linearGradient id={gradientId} x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stopColor="currentColor" stopOpacity="0.25" />
        <stop offset="100%" stopColor="currentColor" stopOpacity="0" />
      </linearGradient></defs>
      <path d={`M0 ${HEIGHT * 0.25} H${WIDTH} M0 ${HEIGHT * 0.5} H${WIDTH} M0 ${HEIGHT * 0.75} H${WIDTH}`} className="overview-curve-grid" />
      <path d={area} fill={`url(#${gradientId})`} />
      <path d={line} className="overview-curve-line" />
      {hovered !== null && <>
        <path d={`M${selected.x} 0 V${HEIGHT}`} className="overview-curve-crosshair" />
        <circle cx={selected.x} cy={selected.y} r="5" className="overview-curve-dot" />
      </>}
    </svg>
    <div className="overview-curve-ends"><span>{time(first.source.at)}</span><span>{time(last.source.at)}</span></div>
  </div>
}
