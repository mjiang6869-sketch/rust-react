// 回测面板。
//
// # 这个面板的核心不是 P&L，是「结论可信度」
//
// 零手续费做市下，回测的 P&L 是成交假设的函数。同一份策略在同一份数据上，
// 乐观模型（触价即成交）和诚实模型（要求真实成交发生在我们的价位）可以给出
// 符号相反的结论。
//
// 所以裁决块放在**最上面**，且不可折叠。只显示 P&L 而把「结论是否可信」
// 藏在下面，会让人把依赖费率活动的结果当成真实 edge。

import { useCallback, useEffect, useState } from 'react'

import { SelectField, DateField } from '../components/FormControls'
import { api } from '../api/client'
import type { BacktestJobSnapshot, BacktestResult, BacktestRunSummary, StrategyInfo } from '../api/types'
import { duration, num, pct, pnlClass, signed, time } from '../format'
import { useAction } from '../state/store'
import { BacktestRunsTable } from './OrdersPanel'
import { EquityCurve } from '../chart/EquityCurve'

interface Props {
  symbol: string
  initialEquity: string
}

export function BacktestPanel({ symbol, initialEquity }: Props) {
  const action = useAction()
  const [strategies, setStrategies] = useState<StrategyInfo[]>([])
  const [strategy, setStrategy] = useState('range_maker')
  const [from, setFrom] = useState('2026-08-01')
  const [to, setTo] = useState('2026-08-07')
  const [models, setModels] = useState<string[]>(['m0', 'm1'])
  const [symbols, setSymbols] = useState<string[]>([symbol])
  const [selectedSymbol, setSelectedSymbol] = useState(symbol)
  const [result, setResult] = useState<BacktestResult | null>(null)
  const [runs, setRuns] = useState<BacktestRunSummary[]>([])
  const [job, setJob] = useState<BacktestJobSnapshot | null>(null)

  const loadHistory = useCallback(async () => {
    const r = await action.run(() => api.backtests(selectedSymbol))
    if (r !== undefined) setRuns(r)
  }, [action, selectedSymbol])

  useEffect(() => {
    api.strategies().then(setStrategies).catch(() => setStrategies([]))
    api.symbols().then((values) => {
      const next = [...new Set([symbol, ...values])]
      setSymbols(next)
      if (!next.includes(selectedSymbol)) setSelectedSymbol(next[0] ?? symbol)
    }).catch(() => setSymbols([symbol]))
    void loadHistory()
    // eslint-disable-next-line react-hooks/exhaustive-deps
    // `useAction()` intentionally returns a per-render facade; including it here
    // would refetch forever while the progress snapshot updates.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [symbol, selectedSymbol])

  const run = useCallback(async () => {
    const started = await action.run(() =>
      api.runBacktest({
        symbol: selectedSymbol,
        strategy,
        from,
        to,
        fill_models: models,
        initial_equity: initialEquity,
      }),
    )
    if (started !== undefined) {
      setJob({ run_id: started.run_id, state: 'running', symbol: selectedSymbol, strategy_id: strategy, model: null, done: 0, total: 0, started_at: null, finished_at: null, error: null, result: null })
      for (;;) {
        await new Promise((resolve) => window.setTimeout(resolve, 500))
        const snapshot = await api.backtestStatus(started.run_id)
        setJob(snapshot)
        if (snapshot.state === 'finished') {
          setResult(snapshot.result)
          void loadHistory()
          break
        }
        if (snapshot.state === 'failed') {
          break
        }
      }
    }
  }, [selectedSymbol, strategy, from, to, models, initialEquity, action, loadHistory])

  return (
    <div className="backtest-layout">
      <section className="panel" aria-labelledby="bt-title">
        <h2 id="bt-title">运行回测</h2>

        <div className="row">
          <SelectField id="bt-symbol" label="交易对" value={selectedSymbol} onChange={setSelectedSymbol}
            options={symbols.map((value) => ({ value, label: value }))} />
          <SelectField id="bt-strategy" label="策略" value={strategy} onChange={setStrategy}
            options={strategies.map((s) => ({ value: s.id, label: s.name }))} />
        </div>
        <div className="row">
          <DateField id="bt-from" label="起始日期" value={from} onChange={setFrom} max={to} />
          <DateField id="bt-to" label="结束日期" value={to} onChange={setTo} min={from}
            error={from > to ? '结束日期不能早于起始日期' : undefined} />
        </div>

        <fieldset className="fieldset">
          <legend>成交模型</legend>
          <p className="muted small">
            必须同时跑 M0 与 M1 才能判断结论是否依赖不现实的成交假设。
          </p>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={models.includes('m0')}
              onChange={(e) =>
                setModels((prev) =>
                  e.target.checked ? [...prev, 'm0'] : prev.filter((x) => x !== 'm0'),
                )
              }
            />
            <span>
              M0 上界（触价即成交，不现实）
              <small className="muted"> — 仅作对照</small>
            </span>
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={models.includes('m1')}
              onChange={(e) =>
                setModels((prev) =>
                  e.target.checked ? [...prev, 'm1'] : prev.filter((x) => x !== 'm1'),
                )
              }
            />
            <span>
              M1 保守下界（诚实基线）
              <small className="muted"> — 要求真实成交发生在我们的价位</small>
            </span>
          </label>
        </fieldset>

        {action.error !== null && (
          <p className="notice notice-error" role="alert">
            {action.error}
          </p>
        )}

        <div className="actions">
          <button
            type="button"
            className="primary"
            onClick={() => void run()}
            disabled={action.busy || job?.state === 'running' || models.length === 0 || !from || !to || from > to}
          >
            {action.busy || job?.state === 'running' ? '回测中…' : '运行回测'}
          </button>
        </div>
        {job?.state === 'running' && (
          <div className="backtest-progress" role="status">
            <div className="panel-head"><strong>回测进行中</strong><span className="muted">{job.model ?? '准备数据'}</span></div>
            <progress max={job.total || 1} value={job.done} />
            <small className="muted">{job.done} / {job.total || '…'} 个分片/模型</small>
          </div>
        )}
      </section>

      <section className="panel" aria-labelledby="bt-results">
        <h2 id="bt-results">回测结果</h2>

        {result === null ? (
          <p className="muted">
            还没有运行。回测需要本地已下载对应区间的数据。
          </p>
        ) : (
          <>
            {/* 裁决块放最上面且不可折叠 */}
            <VerdictBlock result={result} />

            <div className="panel-head sub-head">
              <h3>各成交模型</h3>
              <span className="muted">
                {result.from} → {result.to}（{result.candle_count} 根 K 线）
              </span>
            </div>
            <div className="table-wrap">
              <table className="data-table">
                <thead>
                  <tr>
                    <th scope="col">模型</th>
                    <th scope="col" className="num">
                      盈亏
                    </th>
                    <th scope="col" className="num">
                      成交数
                    </th>
                    <th scope="col" className="num">
                      胜率
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {result.models.map((m) => (
                    <tr key={m.name}>
                      <td>
                        <span className="mono">{m.name}</span>
                        <br />
                        <small
                          className={
                            m.optimism === 'UPPER_BOUND' ? 'muted' : 'muted'
                          }
                        >
                          {m.optimism === 'UPPER_BOUND' ? '上界' : '保守下界'}
                        </small>
                      </td>
                      <td className={`num mono ${pnlClass(m.pnl)}`}>
                        {signed(m.pnl)}
                      </td>
                      <td className="num">{m.trade_count}</td>
                      <td className="num">
                        {m.win_rate === null ? '—' : pct(m.win_rate, 1)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            {result.models.map((m) => (
              <section className="backtest-model-detail" key={`${m.name}-detail`} aria-labelledby={`model-${m.name}`}>
                <div className="panel-head sub-head">
                  <h3 id={`model-${m.name}`}>{m.name} 明细</h3>
                  <span className={m.liquidated ? 'tag-bad' : 'muted'}>{m.liquidated ? '已爆仓并停止' : `年化 ${m.annualized_return === null ? '—' : pct(m.annualized_return, 2)}`}</span>
                </div>
                <div className="backtest-kpis">
                  <span>累计盈亏 <strong className={pnlClass(m.cumulative_pnl)}>{signed(m.cumulative_pnl)}</strong></span>
                  <span>最终权益 <strong>{num(m.final_equity, 2)}</strong></span>
                  <span>订单数 <strong>{m.trades.length}</strong></span>
                </div>
                <EquityCurve points={m.equity_curve} asset={result.symbol.endsWith('USDT') ? 'USDT' : 'USDC'} trend={pnlClass(m.pnl)} />
                <div className="table-wrap">
                  <table className="data-table">
                    <thead><tr><th>入场</th><th>出场</th><th>方向</th><th className="num">数量</th><th className="num">入场价</th><th className="num">出场价</th><th>原因</th><th className="num">盈亏</th></tr></thead>
                    <tbody>{m.trades.map((trade, index) => <tr key={`${m.name}-${trade.entry_at}-${index}`}>
                      <td className="mono">{time(trade.entry_at)}</td><td className="mono">{time(trade.exit_at)}</td><td>{trade.side}</td>
                      <td className="num mono">{trade.quantity}</td><td className="num mono">{trade.entry_price}</td><td className="num mono">{trade.exit_price}</td>
                      <td>{trade.exit_reason}</td><td className={`num mono ${pnlClass(trade.pnl)}`}>{signed(trade.pnl)}</td>
                    </tr>)}</tbody>
                  </table>
                </div>
              </section>
            ))}
          </>
        )}
      </section>

      <section className="panel" aria-labelledby="bt-history">
        <div className="panel-head">
          <h2 id="bt-history">历史回测</h2>
          <button
            type="button"
            className="link-btn"
            onClick={() => void loadHistory()}
            disabled={action.busy}
          >
            刷新
          </button>
        </div>
        <BacktestRunsTable runs={runs} />
      </section>
    </div>
  )
}

/** 结论可信度。这是回测最重要的输出。 */
function VerdictBlock({ result }: { result: BacktestResult }) {
  const v = result.verdict
  const cls = v.conclusive ? 'verdict verdict-ok' : 'verdict verdict-bad'

  return (
    <div className={cls}>
      <div className="verdict-head">
        <span className="verdict-title">结论可信度</span>
        <span className={v.conclusive ? 'tag-ok' : 'tag-bad'}>
          {v.conclusive ? '可用于决策' : '不可用于决策'}
        </span>
      </div>

      <p className="verdict-message">{v.message}</p>

      <dl className="kv">
        {v.sign_flips && (
          <div>
            <dt>符号翻转</dt>
            <dd className="neg">乐观模型与诚实模型方向相反</dd>
          </div>
        )}
        {v.breakeven_fill_rate !== null && (
          <div>
            <dt>盈亏平衡成交率</dt>
            <dd>
              {pct(v.breakeven_fill_rate, 1)}
              <small className="muted">
                {' '}
                — M1 需达到 M0 假设成交量的这个比例才不亏
              </small>
            </dd>
          </div>
        )}
        {v.markout_5s !== null && (
          <div>
            <dt>5 秒 markout 均值</dt>
            <dd className={pnlClass(v.markout_5s)}>
              {signed(v.markout_5s, 4)}
              <small className="muted">
                {' '}
                — 负数表示成交后价格朝不利方向走（被逆向选择）
              </small>
            </dd>
          </div>
        )}
        {v.stop_exposure_events > 0 && (
          <div>
            <dt>止损裸露</dt>
            <dd className="warn">
              {v.stop_exposure_events} 次触发未成交，最长 {duration(v.max_exposure_secs)}
            </dd>
          </div>
        )}
        {v.fee_incomplete && (
          <div>
            <dt>费率来源</dt>
            <dd className="warn">未经交易所对账，结果标记为不完整</dd>
          </div>
        )}
        <div>
          <dt>0% 费率盈亏</dt>
          <dd className={pnlClass(v.pnl_at_promotional_fee)}>
            {signed(v.pnl_at_promotional_fee)}
          </dd>
        </div>
        <div>
          <dt>常规费率盈亏</dt>
          <dd className={pnlClass(v.pnl_at_standard_fee)}>
            {signed(v.pnl_at_standard_fee)}
            <small className="muted"> — 差额即为零费率活动的贡献</small>
          </dd>
        </div>
      </dl>
    </div>
  )
}
