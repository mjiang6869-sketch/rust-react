// 持仓与保护单。
//
// # 必须显示的东西
//
// 做市的持仓有三个信息不可省略，否则用户无法判断风险：
//
// 1. **各档止盈是否已成交** —— 分批止盈的核心状态。只显示"持仓量"看不出
//    第几档已经平掉了。
// 2. **止损挂在哪** —— 以及它是否已触发但未成交（裸露中）。
// 3. **距估算强平的缓冲** —— maker-only 下止损可能不成交，此时强平距离就是
//    真实的风险上限。

import { useCallback, useState } from 'react'

import { api } from '../api/client'
import type { PositionInfo } from '../api/types'
import { num, pnlClass, signed } from '../format'
import { useAction } from '../state/store'

interface Props {
  position: PositionInfo | null
  symbol: string
  compact?: boolean
}

export function PositionCard({ position, symbol, compact = false }: Props) {
  const action = useAction()
  const [confirming, setConfirming] = useState(false)

  const close = useCallback(async () => {
    const r = await action.run(() => api.closePosition())
    if (r !== undefined) setConfirming(false)
  }, [action])

  if (position === null) {
    return (
      <section className="panel" aria-labelledby="pos-title">
        <h2 id="pos-title">当前持仓</h2>
        <p className="muted">当前无持仓（{symbol}）。</p>
      </section>
    )
  }

  const isLong = position.side === 'BUY'
  const filledRungs = position.rungs.filter((r) => r.filled).length

  return (
    <section className="panel" aria-labelledby="pos-title">
      <div className="panel-head">
        <h2 id="pos-title">当前持仓</h2>
        <span className={`side-badge ${isLong ? 'side-buy' : 'side-sell'}`}>
          {position.side_label}
        </span>
      </div>

      {compact ? (
        <div className="table-wrap">
          <table className="data-table">
            <thead><tr>
              <th>合约</th><th>方向</th><th>数量</th><th>开仓价格</th><th>未实现盈亏</th><th>止损价格</th>
            </tr></thead>
            <tbody><tr>
              <td>{symbol}</td><td>{position.side_label}</td><td className="mono">{num(position.quantity)}</td>
              <td className="mono">{num(position.entry_price)}</td>
              <td className={`mono ${pnlClass(position.unrealized_pnl)}`}>{signed(position.unrealized_pnl)}</td>
              <td className="mono">{num(position.stop_price)}{position.stop_triggered && <span className="inline-warn"> 已触发未成交</span>}</td>
            </tr></tbody>
          </table>
        </div>
      ) : (
      <dl className="kv">
        <div>
          <dt>数量</dt>
          <dd>{num(position.quantity)}</dd>
        </div>
        <div>
          <dt>入场价</dt>
          <dd>{num(position.entry_price)}</dd>
        </div>
        <div>
          <dt>未实现盈亏</dt>
          <dd className={pnlClass(position.unrealized_pnl)}>
            {signed(position.unrealized_pnl)}
          </dd>
        </div>
        <div>
          <dt>止损价</dt>
          <dd className={position.stop_triggered ? 'neg' : ''}>
            {position.stop_price === null ? '—' : num(position.stop_price)}
            {position.stop_triggered && (
              <span className="inline-warn"> 已触发未成交</span>
            )}
          </dd>
        </div>
      </dl>

      )}

      {/* 分批止盈状态。这是"分批"能力的可视化——用户要能看出第几档已成交。 */}
      <div className="rungs-status">
        <div className="rungs-status-head">
          <span>分批止盈</span>
          <span className="muted">
            {filledRungs} / {position.rungs.length} 档已成交
          </span>
        </div>
        {position.rungs.map((r) => (
          <div
            key={r.rung}
            className={`rung-status ${r.filled ? 'rung-filled' : 'rung-pending'}`}
          >
            <span className="rung-status-index">第 {r.index} 档</span>
            <span className="rung-status-price">{num(r.price)}</span>
            <span className="rung-status-dist">{num(r.distance_bp, 2)} bp</span>
            <span className="rung-status-frac">
              平 {num(r.fraction) === '—' ? '—' : `${Number(r.fraction) * 100}%`}
            </span>
            <span className="rung-status-state">
              {r.filled ? '已成交' : '挂单中'}
            </span>
          </div>
        ))}
      </div>

      {action.error !== null && (
        <p className="notice notice-error" role="alert">
          {action.error}
        </p>
      )}

      <div className="actions">
        {confirming ? (
          <>
            <button
              type="button"
              className="danger"
              onClick={() => void close()}
              disabled={action.busy}
            >
              {action.busy ? '平仓中…' : '确认市价平仓'}
            </button>
            <button
              type="button"
              className="link-btn"
              onClick={() => setConfirming(false)}
            >
              取消
            </button>
          </>
        ) : (
          <button type="button" className="secondary" onClick={() => setConfirming(true)}>
            手动平仓
          </button>
        )}
      </div>

      {confirming && (
        <p className="notice notice-warn">
          手动平仓按市价语义成交，会被收取 taker 手续费（
          {num('0.0005', 4)} 量级），与挂单的零费率不同。
        </p>
      )}
    </section>
  )
}
