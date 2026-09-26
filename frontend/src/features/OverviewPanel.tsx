import { useEffect, useState } from 'react'
import { ArrowUpRight, RefreshCw } from 'lucide-react'

import { api } from '../api/client'
import type { EngineState, OverviewData } from '../api/types'
import { EquityCurve } from '../chart/EquityCurve'
import { num, pnlClass, signed, time } from '../format'
import { OrdersPanel } from './OrdersPanel'
import { PositionCard } from './PositionCard'
import { ProfitCalendar } from './ProfitCalendar'

function estimateReason(data: OverviewData | null, days: number): string {
  if (data === null) return '暂无收益快照'
  if (!data.fee_is_authoritative) return '费率未对账，暂不估算'
  return `需要连续 ${days} 天账户快照`
}

export function OverviewPanel({ engine, onGoToMarket }: { engine: EngineState; onGoToMarket: () => void }) {
  const [data, setData] = useState<OverviewData | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [refreshKey, setRefreshKey] = useState(0)

  useEffect(() => {
    if (engine.mode === 'LIVE') return
    // 本地账户快照接口；页面可见时低频刷新，不触发币安 REST 请求。
    const timer = window.setInterval(() => setRefreshKey((key) => key + 1), 60_000)
    return () => window.clearInterval(timer)
  }, [engine.mode])

  useEffect(() => {
    if (engine.mode === 'LIVE') {
      setData(null)
      setError('实盘模式下不展示模拟盘收益记录')
      return
    }
    let active = true
    setLoading(true)
    void api.overview().then((result) => {
      if (!active) return
      setData(result)
      setError(null)
    }).catch((cause: unknown) => {
      if (!active) return
      setError(cause instanceof Error ? cause.message : '收益数据读取失败')
    }).finally(() => {
      if (active) setLoading(false)
    })
    return () => { active = false }
  }, [engine.mode, engine.symbol, refreshKey])

  const asset = data?.settlement_asset ?? engine.instrument.settlement_asset
  const equity = data?.equity ?? engine.equity
  const realized = data?.realized_pnl ?? engine.realized_pnl
  const unrealized = data?.unrealized_pnl ?? engine.unrealized_pnl
  const cumulative = data?.cumulative_pnl ?? null
  const monthEstimate = data?.estimated_month_pnl ?? null
  const annualEstimate = data?.estimated_annualized_pct ?? null
  const cumulativeClass = pnlClass(cumulative)

  return <div className="overview-dashboard">
    <div className="overview-utility">
      <span><b>{engine.symbol}</b><span className="overview-utility-separator">/</span>{asset}<span className={`mode-badge ${engine.mode === 'LIVE' ? 'mode-live' : 'mode-paper'}`}>{engine.mode_label}</span></span>
      <button type="button" className="overview-refresh" onClick={() => setRefreshKey((key) => key + 1)} disabled={loading || engine.mode === 'LIVE'}>
        <RefreshCw size={15} aria-hidden="true" /> {loading ? '更新中' : '刷新收益'}
      </button>
    </div>

    {error && <div className="notice notice-warn overview-error" role="status">{error}。当前权益仍以账户实时状态显示。</div>}

    <section className="overview-metrics" aria-label="账户收益概览">
      <div className="overview-equity-card">
        <span className="overview-metric-label">账户权益 <small>{asset}</small></span>
        <strong className="overview-equity-value">{num(equity, 2)}</strong>
        <div className="overview-equity-breakdown">
          <span>已实现 <b className={pnlClass(realized)}>{signed(realized, 2)}</b></span>
          <span>未实现 <b className={pnlClass(unrealized)}>{signed(unrealized, 2)}</b></span>
        </div>
      </div>
      <div className="overview-metric-card">
        <span className="overview-metric-label">累计收益</span>
        <strong className={cumulativeClass}>{signed(cumulative, 2)} <small>{asset}</small></strong>
        <span className={`overview-metric-sub ${cumulativeClass}`}>{data?.cumulative_return_pct === null || data === null ? '基于初始权益' : `${signed(data.cumulative_return_pct, 2)}% · 基于初始权益`}</span>
      </div>
      <div className="overview-metric-card">
        <span className="overview-metric-label">预估本月收益</span>
        <strong className={pnlClass(monthEstimate)}>{signed(monthEstimate, 2)} {monthEstimate !== null && <small>{asset}</small>}</strong>
        <span className="overview-metric-sub">{monthEstimate === null ? estimateReason(data, 7) : '本月日均已实现盈亏推算'}</span>
      </div>
      <div className="overview-metric-card">
        <span className="overview-metric-label">预估年化率</span>
        <strong className={pnlClass(annualEstimate)}>{annualEstimate === null ? '—' : `${signed(annualEstimate, 2)}%`}</strong>
        <span className="overview-metric-sub">{annualEstimate === null ? estimateReason(data, 30) : '近 30 日已实现盈亏线性推算'}</span>
      </div>
    </section>

    <div className="overview-visuals">
      <section className="panel overview-curve-panel" aria-labelledby="overview-curve-title">
        <div className="overview-section-head">
          <div><h2 id="overview-curve-title">资金曲线</h2><span>本次模拟盘运行 · {asset}</span></div>
          <span className="overview-section-asof">{data ? `更新于 ${time(data.as_of)}` : '等待账户快照'}</span>
        </div>
        <EquityCurve points={data?.curve ?? []} asset={asset} trend={cumulativeClass} />
        <p className="overview-curve-note">账户权益包含未实现盈亏；每 15 分钟保存快照，刷新时附加当前值。</p>
      </section>
      <ProfitCalendar daily={data?.daily ?? []} asOf={data?.as_of ?? new Date().toISOString()} asset={asset} />
    </div>

    <div className="overview-operations">
      <PositionCard position={engine.position} symbol={engine.symbol} />
      <section className="panel overview-orders" aria-labelledby="overview-orders-title">
        <div className="panel-head"><h2 id="overview-orders-title">在途订单 <span className="overview-count">{engine.open_orders.length}</span></h2>
          <button type="button" className="link-btn" onClick={onGoToMarket}>去看盘 <ArrowUpRight size={14} aria-hidden="true" /></button>
        </div>
        {engine.open_orders.length === 0 ? <div className="panel-empty">没有在途订单</div> :
          <OrdersPanel orders={engine.open_orders} symbol={engine.symbol} compact title="" />}
      </section>
    </div>

    <div className="overview-meta">
      {data && <span>数据来源：本次运行的模拟盘账户快照</span>}
      <span className={engine.instrument.fee_is_authoritative ? '' : 'warn'}>费率：{engine.instrument.fee_is_authoritative ? '已对账' : '未对账，预测指标暂停'}</span>
    </div>
  </div>
}
