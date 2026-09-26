import { useState } from 'react'
import { ChevronLeft, ChevronRight } from 'lucide-react'

import type { OverviewData } from '../api/types'
import { pnlClass, signed } from '../format'

type Day = OverviewData['daily'][number]

function shanghaiMonth(iso: string): number {
  const date = new Date(new Date(iso).getTime() + 8 * 60 * 60 * 1000)
  return date.getUTCFullYear() * 12 + date.getUTCMonth()
}

function dateKey(year: number, month: number, day: number): string {
  return `${year}-${String(month).padStart(2, '0')}-${String(day).padStart(2, '0')}`
}

export function ProfitCalendar({ daily, asOf, asset }: { daily: Day[]; asOf: string; asset: string }) {
  const currentMonth = shanghaiMonth(asOf)
  const firstMonth = daily[0]
    ? Number(daily[0].date.slice(0, 4)) * 12 + Number(daily[0].date.slice(5, 7)) - 1
    : currentMonth
  const [month, setMonth] = useState(currentMonth)
  const visibleMonth = Math.max(firstMonth, Math.min(currentMonth, month))
  const year = Math.floor(visibleMonth / 12)
  const monthNumber = visibleMonth % 12 + 1
  const dayCount = new Date(Date.UTC(year, monthNumber, 0)).getUTCDate()
  const offset = (new Date(Date.UTC(year, monthNumber - 1, 1)).getUTCDay() + 6) % 7
  const byDate = new Map(daily.map((day) => [day.date, day]))
  const cells = Array.from({ length: Math.ceil((offset + dayCount) / 7) * 7 }, (_, index) => {
    const dayNumber = index - offset + 1
    return dayNumber >= 1 && dayNumber <= dayCount ? { dayNumber, key: dateKey(year, monthNumber, dayNumber) } : null
  })
  const hasSamples = daily.length > 0

  return <section className="panel overview-calendar-panel" aria-labelledby="overview-calendar-title">
    <div className="overview-section-head">
      <div>
        <h2 id="overview-calendar-title">收益日历</h2>
        <span>已实现盈亏 · UTC+8</span>
      </div>
      <div className="overview-calendar-nav">
        <button type="button" aria-label="上个月" onClick={() => setMonth(visibleMonth - 1)} disabled={visibleMonth <= firstMonth}><ChevronLeft size={17} /></button>
        <strong>{year} 年 {monthNumber} 月</strong>
        <button type="button" aria-label="下个月" onClick={() => setMonth(visibleMonth + 1)} disabled={visibleMonth >= currentMonth}><ChevronRight size={17} /></button>
      </div>
    </div>
    <div className="overview-calendar-body">
      <div className="overview-calendar-grid" role="grid" aria-label={`${year} 年 ${monthNumber} 月收益`}>
        {['一', '二', '三', '四', '五', '六', '日'].map((name) => <span className="overview-calendar-weekday" key={name}>{name}</span>)}
        {cells.map((cell, index) => {
          if (cell === null) return <span key={`blank-${index}`} className="overview-calendar-blank" />
          const day = byDate.get(cell.key)
          const direction = day ? pnlClass(day.realized_pnl) : ''
          return <div key={cell.key}
            className={`overview-calendar-day ${day ? `sampled ${direction} level-${day.intensity}` : ''}`}
            title={day ? `${cell.key}：已实现 ${signed(day.realized_pnl, 2)} ${asset}` : `${cell.key}：无快照`}
            aria-label={day ? `${cell.key} 已实现 ${signed(day.realized_pnl, 2)} ${asset}` : `${cell.key} 无快照`}>
            <span>{cell.dayNumber}</span>
            {day && <small>{day.intensity === 0 ? '0' : signed(day.realized_pnl, 2)}</small>}
          </div>
        })}
      </div>
      <p className="overview-calendar-note">{hasSamples
        ? `按账户快照归属日期，非逐笔成交账单；无记录的日期留空。金额单位 ${asset}。`
        : '从本次模拟盘运行开始记录，暂无历史日期。'}</p>
    </div>
  </section>
}
