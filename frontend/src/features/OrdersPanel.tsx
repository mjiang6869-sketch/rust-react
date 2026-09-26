// 在途订单与成交历史。
//
// # 表格必须稳定
//
// 订单号与价格都是变长字符串。列宽不稳定会让行高跳动、长订单号撑破布局。
// 所以：等宽字体 + 固定列宽 + 长字段允许中间省略（`title` 属性给完整值）。

import { useCallback, useEffect, useState } from 'react'

import { api } from '../api/client'
import type { BacktestRunSummary, FillRecord, OrderInfo } from '../api/types'
import { num, pnlClass, qty, signed, time } from '../format'
import { useAction } from '../state/store'

interface Props {
  orders: OrderInfo[]
  title: string
  /** 数据所属交易对。成交历史按它查询。 */
  symbol: string
  compact?: boolean
}

export function OrdersPanel({ orders, title, symbol, compact = false }: Props) {
  const action = useAction()
  const cancelAction = useAction()
  const [cancelNotice, setCancelNotice] = useState<string | null>(null)

  // 成交历史需要单独请求（不在引擎状态里）。
  const [fills, setFills] = useState<FillRecord[]>([])
  const [showFills, setShowFills] = useState(!compact)

  const loadFills = useCallback(async () => {
    const r = await action.run(() => api.fills({ symbol }))
    if (r !== undefined) setFills(r)
  }, [action, symbol])

  useEffect(() => {
    if (showFills) void loadFills()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [showFills])

  const handleCancel = useCallback(
    async (order: OrderInfo) => {
      setCancelNotice(null)
      const r = await cancelAction.run(() => api.cancelPending(order.client_id))
      if (r?.cancelled === true && order.source === 'STRATEGY') {
        setCancelNotice('自动化做市会在下一根 K 线收盘时重新评估。')
      }
    },
    [cancelAction],
  )

  return (
    <section className="panel" aria-labelledby={`orders-${title}`}>
      <div className="panel-head">
        <h2 id={`orders-${title}`}>{title}</h2>
        <span className="muted">{orders.length} 张</span>
      </div>

      {cancelAction.error !== null && (
        <p className="notice notice-error" role="alert">
          {cancelAction.error}
        </p>
      )}
      {cancelNotice !== null && (
        <p className="notice notice-info" role="status">
          {cancelNotice}
        </p>
      )}

      {orders.length === 0 ? (
        <p className="muted">当前没有在途订单。</p>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                <th scope="col">用途</th>
                <th scope="col">方向</th>
                <th scope="col" className="num">
                  价格
                </th>
                {!compact && (
                  <>
                    <th scope="col" className="num">
                      数量
                    </th>
                    <th scope="col" className="num">
                      已成交
                    </th>
                  </>
                )}
                <th scope="col">状态</th>
                <th scope="col">来源</th>
                {!compact && <th scope="col">订单号</th>}
                <th scope="col">操作</th>
              </tr>
            </thead>
            <tbody>
              {orders.map((o) => (
                <tr key={o.client_id}>
                  <td>{o.purpose_label}</td>
                  <td className={o.side === 'BUY' ? 'pos' : 'neg'}>
                    {o.side === 'BUY' ? '买' : '卖'}
                  </td>
                  <td className="num mono">{num(o.limit_price)}</td>
                  {!compact && (
                    <>
                      <td className="num mono">{qty(o.quantity)}</td>
                      <td className="num mono">{qty(o.filled)}</td>
                    </>
                  )}
                  <td>
                    <span className="state-tag">{o.state}</span>
                    {o.expires_at !== null && (
                      <div className="muted small">到期：{time(o.expires_at)}</div>
                    )}
                  </td>
                  <td>{o.source_label ?? '—'}</td>
                  {!compact && (
                    <td className="mono ellipsis" title={o.client_id}>
                      {o.client_id}
                    </td>
                  )}
                  <td>
                    {o.cancellable ? (
                      <button
                        type="button"
                        className="link-btn"
                        disabled={cancelAction.busy}
                        onClick={() => void handleCancel(o)}
                      >
                        撤单
                      </button>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {compact && (
        <button
          type="button"
          className="link-btn"
          onClick={() => setShowFills((v) => !v)}
        >
          {showFills ? '收起成交历史' : '查看成交历史'}
        </button>
      )}

      {showFills && (
        <>
          <div className="panel-head sub-head">
            <h3>成交历史</h3>
            <button
              type="button"
              className="link-btn"
              onClick={() => void loadFills()}
              disabled={action.busy}
            >
              刷新
            </button>
          </div>
          {action.error !== null && (
            <p className="notice notice-error" role="alert">
              {action.error}
            </p>
          )}
          {fills.length === 0 ? (
            <p className="muted">还没有成交记录。</p>
          ) : (
            <div className="table-wrap">
              <table className="data-table">
                <thead>
                  <tr>
                    <th scope="col">时刻</th>
                    <th scope="col" className="num">
                      价格
                    </th>
                    <th scope="col" className="num">
                      数量
                    </th>
                    <th scope="col" className="num">
                      手续费
                    </th>
                    <th scope="col">资产</th>
                  </tr>
                </thead>
                <tbody>
                  {fills.map((f) => (
                    <tr key={f.trade_id}>
                      <td>{time(f.at)}</td>
                      <td className="num mono">{num(f.price)}</td>
                      <td className="num mono">{qty(f.quantity)}</td>
                      <td className={`num mono ${pnlClass(f.fee)}`}>
                        {signed(f.fee, 6)}
                      </td>
                      <td>{f.fee_asset}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
    </section>
  )
}

/** 回测历史表（回测面板复用）。 */
export function BacktestRunsTable({ runs }: { runs: BacktestRunSummary[] }) {
  if (runs.length === 0) {
    return <p className="muted">还没有回测记录。</p>
  }
  return (
    <div className="table-wrap">
      <table className="data-table">
        <thead>
          <tr>
            <th scope="col">交易对</th>
            <th scope="col">策略</th>
            <th scope="col" className="num">
              盈亏
            </th>
            <th scope="col" className="num">
              成交数
            </th>
            <th scope="col">结论</th>
          </tr>
        </thead>
        <tbody>
          {runs.map((r) => {
            const pnl = signed(
              (Number(r.final_equity) - Number(r.initial_equity)).toString(),
            )
            return (
              <tr key={r.run_id}>
                <td>{r.symbol}</td>
                <td>{r.strategy_id}</td>
                <td className={`num mono ${pnlClass(pnl)}`}>{pnl}</td>
                <td className="num">{r.trade_count}</td>
                <td className={r.actionable ? 'pos' : 'warn'}>{r.verdict}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}
