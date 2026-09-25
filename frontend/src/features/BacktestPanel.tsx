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

import { api } from '../api/client'
import type { BacktestResult, BacktestRunSummary, StrategyInfo } from '../api/types'
import { duration, pct, pnlClass, signed } from '../format'
import { useAction } from '../state/store'
import { BacktestRunsTable } from './OrdersPanel'

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
  const [result, setResult] = useState<BacktestResult | null>(null)
  const [runs, setRuns] = useState<BacktestRunSummary[]>([])

  const loadHistory = useCallback(async () => {
    const r = await action.run(() => api.backtests(symbol))
    if (r !== undefined) setRuns(r)
  }, [action, symbol])

  useEffect(() => {
    api.strategies().then(setStrategies).catch(() => setStrategies([]))
    void loadHistory()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const run = useCallback(async () => {
    const r = await action.run(() =>
      api.runBacktest({
        symbol,
        strategy,
        from,
        to,
        fill_models: models,
        initial_equity: initialEquity,
      }),
    )
    if (r !== undefined) {
      setResult(r)
      void loadHistory()
    }
  }, [symbol, strategy, from, to, models, initialEquity, action, loadHistory])

  return (
    <div className="backtest-layout">
      <section className="panel" aria-labelledby="bt-title">
        <h2 id="bt-title">运行回测</h2>

        <div className="row">
          <div className="field">
            <label htmlFor="bt-strategy">策略</label>
            <div className="input-wrap">
              <select
                id="bt-strategy"
                value={strategy}
                onChange={(e) => setStrategy(e.target.value)}
              >
                {strategies.map((s) => (
                  <option key={s.id} value={s.id}>
                    {s.name}
                  </option>
                ))}
              </select>
            </div>
          </div>
        </div>

        <div className="row">
          <div className="field">
            <label htmlFor="bt-from">起始日期</label>
            <div className="input-wrap">
              <input
                id="bt-from"
                type="text"
                value={from}
                onChange={(e) => setFrom(e.target.value)}
              />
            </div>
            <small>格式 YYYY-MM-DD</small>
          </div>
          <div className="field">
            <label htmlFor="bt-to">结束日期</label>
            <div className="input-wrap">
              <input
                id="bt-to"
                type="text"
                value={to}
                onChange={(e) => setTo(e.target.value)}
              />
            </div>
          </div>
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
            disabled={action.busy || models.length === 0}
          >
            {action.busy ? '回测中…' : '运行回测'}
          </button>
        </div>
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
