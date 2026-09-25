// 数据管理。
//
// # 为什么这个面板重要
//
// 回测与回放都依赖本地历史数据，而数据下载是几小时级别的长任务。这个面板
// 让下载可以从前端发起，并显示三件必需的事：
//
// 1. **覆盖情况** —— 已有哪些区间、哪些分区异常。没有这个用户不知道该下什么。
// 2. **异常分区** —— 行数不符的分区会被标记为「待查」而非通过。必须显示，
//    否则用户会以为数据完整而实际不完整。
// 3. **缺口** —— 跨越缺口的回测会凭空发明成交，所以缺口必须显式呈现。

import { useCallback, useEffect, useState } from 'react'

import { api } from '../api/client'
import type { Coverage, DatasetCoverage } from '../api/types'
import { bytes } from '../format'
import { useAction, useAppState } from '../state/store'

/** 可选数据集。与后端的 `kind` 参数一致。 */
const KINDS = [
  { key: 'klines', label: 'K 线（1m）', note: '策略信号，体积小' },
  { key: 'agg_trades', label: '逐笔成交', note: '成交模型与 markout，体积最大' },
  { key: 'mark_price', label: '标记价（1m）', note: '强平距离估算' },
  { key: 'funding', label: '资金费率', note: '持仓成本，体积很小' },
] as const

export function DataPanel() {
  const { progress } = useAppState()
  const action = useAction()

  const [coverage, setCoverage] = useState<Coverage | null>(null)
  const [symbols, setSymbols] = useState('ETHUSDC')
  const [kinds, setKinds] = useState<string[]>(['klines', 'agg_trades'])
  const [from, setFrom] = useState('2026-01')
  const [to, setTo] = useState('2026-08')

  const load = useCallback(async () => {
    const r = await action.run(() => api.coverage())
    if (r !== undefined) setCoverage(r)
  }, [action])

  useEffect(() => {
    void load()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const start = useCallback(async () => {
    const symbolList = symbols
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s !== '')
    if (symbolList.length === 0) return
    if (kinds.length === 0) return

    const r = await action.run(() =>
      api.startDownload({ symbols: symbolList, kinds, from, to }),
    )
    if (r !== undefined) {
      // 下载在后台跑，完成后重新拉覆盖情况。
      // 这里不等待——任务可能跑几小时。
      setTimeout(() => void load(), 2000)
    }
  }, [symbols, kinds, from, to, action, load])

  return (
    <div className="data-layout">
      <section className="panel" aria-labelledby="dl-title">
        <h2 id="dl-title">下载历史数据</h2>

        <div className="row">
          <div className="field">
            <label htmlFor="dl-symbols">交易对</label>
            <div className="input-wrap">
              <input
                id="dl-symbols"
                type="text"
                value={symbols}
                onChange={(e) => setSymbols(e.target.value)}
                placeholder="ETHUSDC, BTCUSDC"
              />
            </div>
            <small>多个交易对用逗号分隔</small>
          </div>
        </div>

        <fieldset className="fieldset">
          <legend>数据集</legend>
          {KINDS.map((k) => (
            <label className="checkbox" key={k.key}>
              <input
                type="checkbox"
                checked={kinds.includes(k.key)}
                onChange={(e) =>
                  setKinds((prev) =>
                    e.target.checked
                      ? [...prev, k.key]
                      : prev.filter((x) => x !== k.key),
                  )
                }
              />
              <span>
                {k.label}
                <small className="muted"> — {k.note}</small>
              </span>
            </label>
          ))}
        </fieldset>

        <div className="row">
          <div className="field">
            <label htmlFor="dl-from">起始月份</label>
            <div className="input-wrap">
              <input
                id="dl-from"
                type="text"
                value={from}
                onChange={(e) => setFrom(e.target.value)}
                placeholder="2026-01"
              />
            </div>
            <small>格式 YYYY-MM</small>
          </div>
          <div className="field">
            <label htmlFor="dl-to">结束月份</label>
            <div className="input-wrap">
              <input
                id="dl-to"
                type="text"
                value={to}
                onChange={(e) => setTo(e.target.value)}
                placeholder="2026-08"
              />
            </div>
            <small>格式 YYYY-MM</small>
          </div>
        </div>

        {progress?.type === 'download' && (
          <div className="progress">
            <div className="progress-bar">
              <div
                className="progress-fill"
                style={{
                  width: `${progress.total > 0 ? (progress.done / progress.total) * 100 : 0}%`,
                }}
              />
            </div>
            <span className="muted">
              {progress.symbol} {progress.kind} {progress.month} — {progress.done}/
              {progress.total}
            </span>
          </div>
        )}

        {progress?.type === 'download_done' && (
          <p className="notice notice-info">
            下载完成：成功 {progress.completed} 个分区
            {progress.failed > 0 && `，失败 ${progress.failed} 个`}。
            {progress.failed > 0 && ' 失败的分区可以重跑同一次下载，已完成的会跳过。'}
          </p>
        )}

        {action.error !== null && (
          <p className="notice notice-error" role="alert">
            {action.error}
          </p>
        )}

        <div className="actions">
          <button
            type="button"
            className="primary"
            onClick={() => void start()}
            disabled={action.busy}
          >
            {action.busy ? '启动中…' : '开始下载'}
          </button>
          <button
            type="button"
            className="secondary"
            onClick={() => void load()}
            disabled={action.busy}
          >
            刷新覆盖情况
          </button>
        </div>

        <p className="muted small">
          下载在后台进行，可以随时关闭页面。已完成的进度会写入台账，重跑会跳过
          已完成的分区。
        </p>
      </section>

      <section className="panel" aria-labelledby="cov-title">
        <div className="panel-head">
          <h2 id="cov-title">本地数据覆盖</h2>
          {coverage !== null && (
            <span className="muted mono">{coverage.data_root}</span>
          )}
        </div>

        {coverage === null ? (
          <p className="muted">加载中…</p>
        ) : coverage.datasets.length === 0 ? (
          <p className="muted">
            台账为空——尚未下载任何数据。用左侧表单开始第一次下载。
          </p>
        ) : (
          <div className="coverage-list">
            {coverage.datasets.map((d) => (
              <DatasetBlock key={`${d.kind}-${d.symbol}`} data={d} />
            ))}
          </div>
        )}

        {coverage !== null && coverage.gaps.length > 0 && (
          <div className="gaps">
            <h3>记录在案的缺口（{coverage.gaps.length} 处）</h3>
            <p className="notice notice-warn">
              跨越缺口的回测会凭空发明不可能的成交，所以默认会被拒绝运行。
            </p>
            <ul className="gap-list">
              {coverage.gaps.slice(0, 20).map((g, i) => (
                <li key={i}>
                  <span className="mono">
                    {g.symbol} {g.kind}
                  </span>
                  <span className="muted">
                    {g.from} → {g.to}
                  </span>
                  <span>{g.note}</span>
                </li>
              ))}
            </ul>
          </div>
        )}
      </section>
    </div>
  )
}

function DatasetBlock({ data }: { data: DatasetCoverage }) {
  const allOk = data.problems.length === 0 && data.finalized === data.partitions
  return (
    <div className="coverage-item">
      <div className="coverage-head">
        <span className="coverage-symbol">{data.symbol}</span>
        <span className="coverage-kind">{data.kind}</span>
        <span className={allOk ? 'tag-ok' : 'tag-warn'}>
          {allOk ? '完整' : '有问题'}
        </span>
      </div>
      <dl className="kv kv-inline">
        <div>
          <dt>分区</dt>
          <dd>
            {data.finalized} / {data.partitions}
          </dd>
        </div>
        <div>
          <dt>区间</dt>
          <dd>
            {data.first_month ?? '—'} → {data.last_month ?? '—'}
          </dd>
        </div>
        <div>
          <dt>占用</dt>
          <dd>{bytes(data.parquet_bytes)}</dd>
        </div>
      </dl>
      {data.problems.length > 0 && (
        <details className="problems">
          <summary>{data.problems.length} 个异常分区</summary>
          <ul>
            {data.problems.slice(0, 24).map((p, i) => (
              <li key={i} className="mono">
                {p}
              </li>
            ))}
          </ul>
          <p className="muted small">
            异常分区不会被当作可用数据，重跑下载会重新处理它们。
          </p>
        </details>
      )}
    </div>
  )
}
