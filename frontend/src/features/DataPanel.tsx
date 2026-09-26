// 数据管理。
//
// # 为什么这个面板重要
//
// 回测与回放都依赖本地历史数据，而数据下载是几小时级别的长任务。这个面板
// 让下载可以从前端发起，并显示四件必需的事：
//
// 1. **覆盖情况** —— 已有哪些区间、哪些分区异常。没有这个用户不知道该下什么。
// 2. **异常分区** —— 行数不符的分区会被标记为「待查」而非通过。必须显示，
//    否则用户会以为数据完整而实际不完整。
// 3. **缺口** —— 跨越缺口的回测会凭空发明成交，所以缺口必须显式呈现。
// 4. **下载任务进度** —— 任务是单例、跨页面存在的，界面必须能看到当前
//    分区、阶段、已用时长与失败清单，而不只是一个笼统的百分比。

import { useCallback, useEffect, useRef, useState } from 'react'

import { InputField, DateField } from '../components/FormControls'
import { api } from '../api/client'
import type {
  ArchiveRange,
  Coverage,
  DatasetCoverage,
  DownloadJob,
  DownloadJobState,
} from '../api/types'
import { bytes, duration } from '../format'
import { useAction, useAppState } from '../state/store'

/** 可选数据集。与后端的 `kind` 参数一致。 */
const KINDS = [
  { key: 'klines', label: 'K 线（1m）', note: '策略信号，体积小' },
  { key: 'agg_trades', label: '逐笔成交', note: '成交模型与 markout，体积最大' },
  { key: 'mark_price', label: '标记价（1m）', note: '强平距离估算' },
  { key: 'funding', label: '资金费率', note: '持仓成本，体积很小' },
] as const

const STATE_LABELS: Record<DownloadJobState, string> = {
  idle: '空闲',
  running: '进行中',
  finished: '已完成',
  cancelled: '已取消',
  failed: '失败',
}

const STATE_TAG_CLASS: Record<DownloadJobState, string> = {
  idle: 'tag',
  running: 'tag-warn',
  finished: 'tag-ok',
  cancelled: 'tag',
  failed: 'tag-bad',
}

export function DataPanel() {
  const { downloadJob: job, setDownloadJob } = useAppState()
  const loadAction = useAction()
  const startAction = useAction()
  const cancelAction = useAction()
  const rangeAction = useAction()
  const initAction = useAction()

  const [coverage, setCoverage] = useState<Coverage | null>(null)
  const [symbols, setSymbols] = useState('ETHUSDC')
  const [kinds, setKinds] = useState<string[]>(['klines', 'agg_trades'])
  const [from, setFrom] = useState('2026-01')
  const [to, setTo] = useState('2026-08')
  const [ranges, setRanges] = useState<ArchiveRange[] | null>(null)

  const load = useCallback(async () => {
    const r = await loadAction.run(() => api.coverage())
    if (r !== undefined) setCoverage(r)
  }, [loadAction])

  useEffect(() => {
    void load()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // 挂载时用 REST 查一次下载任务快照，初始化界面；之后的更新全部来自
  // WebSocket 的 `download_status` 推送。不这样做的话，打开面板时若已有
  // 任务在跑，界面会在下一次广播到达前误以为空闲。
  useEffect(() => {
    void initAction.run(() => api.downloadStatus()).then((r) => {
      if (r !== undefined) setDownloadJob(r)
    })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // 任务从「进行中」变为结束态时，本地数据已经变化——自动刷新覆盖情况。
  const prevJobState = useRef<DownloadJobState | null>(null)
  useEffect(() => {
    const prev = prevJobState.current
    const cur = job?.state ?? null
    if (prev === 'running' && cur !== null && cur !== 'running') {
      void load()
    }
    prevJobState.current = cur
  }, [job?.state, load])

  // 任务运行期间每秒刷新一次，让「已用时长」能走动。
  const [tick, setTick] = useState(() => Date.now())
  useEffect(() => {
    if (job?.state !== 'running') return
    const id = setInterval(() => setTick(Date.now()), 1000)
    return () => clearInterval(id)
  }, [job?.state])

  const start = useCallback(async () => {
    const symbolList = symbols
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s !== '')
    if (symbolList.length === 0) return
    if (kinds.length === 0) return

    const r = await startAction.run(() =>
      api.startDownload({ symbols: symbolList, kinds, from, to }),
    )
    if (r !== undefined) setDownloadJob(r)
  }, [symbols, kinds, from, to, startAction, setDownloadJob])

  const cancel = useCallback(async () => {
    const r = await cancelAction.run(() => api.cancelDownload())
    if (r !== undefined) {
      // 取消是异步生效的（后台任务需要看到取消令牌才能收尾），这里不用
      // 乐观更新本地状态——等 WS 的 `download_status` 推送即可。
    }
  }, [cancelAction])

  // 「从最早开始」：对当前填写的每个交易对都查一次归档范围，取全部数据集
  // 里最早的作为 from、最晚的作为 to（字符串 YYYY-MM 直接比较即可）。
  const useEarliest = useCallback(async () => {
    const symbolList = symbols
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s !== '')
    if (symbolList.length === 0 || kinds.length === 0) return

    const results = await rangeAction.run(async () => {
      const list: ArchiveRange[] = []
      for (const symbol of symbolList) {
        list.push(await api.archiveRange(symbol, kinds))
      }
      return list
    })
    if (results === undefined) return
    setRanges(results)

    let earliest: string | null = null
    let latest: string | null = null
    for (const r of results) {
      for (const d of r.datasets) {
        if (d.earliest !== null && (earliest === null || d.earliest < earliest)) {
          earliest = d.earliest
        }
        if (d.latest !== null && (latest === null || d.latest > latest)) {
          latest = d.latest
        }
      }
    }
    if (earliest !== null) setFrom(earliest)
    if (latest !== null) setTo(latest)
  }, [symbols, kinds, rangeAction])

  const running = job?.state === 'running'

  return (
    <div className="data-layout">
      <section className="panel" aria-labelledby="dl-title">
        <h2 id="dl-title">下载历史数据</h2>

        <div className="row">
          <InputField id="dl-symbols" label="交易对" value={symbols} onChange={setSymbols}
            placeholder="ETHUSDC, BTCUSDC" hint="多个交易对用逗号分隔" disabled={running} />
        </div>

        <fieldset className="fieldset">
          <legend>数据集</legend>
          {KINDS.map((k) => (
            <label className="checkbox" key={k.key}>
              <input
                type="checkbox"
                checked={kinds.includes(k.key)}
                disabled={running}
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
          <DateField id="dl-from" label="起始月份" mode="month" value={from} onChange={setFrom} max={to} />
          <DateField id="dl-to" label="结束月份" mode="month" value={to} onChange={setTo} min={from}
            error={from > to ? '结束月份不能早于起始月份' : undefined} />
        </div>

        <div className="actions">
          <button
            type="button"
            className="secondary"
            onClick={() => void useEarliest()}
            disabled={rangeAction.busy || running}
          >
            {rangeAction.busy ? '查询中…' : '从最早开始'}
          </button>
        </div>

        {rangeAction.error !== null && (
          <p className="notice notice-error" role="alert">
            {rangeAction.error}
          </p>
        )}

        {ranges !== null && (
          <div className="archive-ranges">
            {ranges.map((r) => (
              <div key={r.symbol} className="archive-range-block">
                <div className="archive-range-head">
                  <span className="mono">{r.symbol}</span>
                </div>
                <ul className="archive-range-list">
                  {r.datasets.map((d) => (
                    <li key={d.kind}>
                      <span>{d.label}</span>
                      {d.error !== null ? (
                        <span className="muted">查询失败：{d.error}</span>
                      ) : (
                        <span className="mono">
                          {d.earliest ?? '—'} → {d.latest ?? '—'}
                        </span>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
            ))}
            <p className="muted small">
              {ranges[0]?.hint} · 来源：{ranges[0]?.source}
            </p>
          </div>
        )}

        {startAction.error !== null && (
          <p className="notice notice-error" role="alert">
            {startAction.error}
          </p>
        )}
        {cancelAction.error !== null && (
          <p className="notice notice-error" role="alert">
            {cancelAction.error}
          </p>
        )}

        <div className="actions">
          <button
            type="button"
            className="primary"
            onClick={() => void start()}
            disabled={startAction.busy || running || !from || !to || from > to}
          >
            {startAction.busy ? '启动中…' : '开始下载'}
          </button>
          {running && (
            <button
              type="button"
              className="danger"
              onClick={() => void cancel()}
              disabled={cancelAction.busy}
            >
              {cancelAction.busy ? '取消中…' : '取消下载'}
            </button>
          )}
          <button
            type="button"
            className="secondary"
            onClick={() => void load()}
            disabled={loadAction.busy}
          >
            刷新覆盖情况
          </button>
        </div>

        <p className="muted small">
          下载在后台进行，可以随时关闭页面。已完成的分区会写入台账，重跑会跳过
          已完成的分区，只重试失败的分区。
        </p>
      </section>

      <section className="panel" aria-labelledby="job-title">
        <h2 id="job-title">下载任务</h2>
        <DownloadJobCard job={job} tick={tick} />
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

/** 下载任务的完整状态展示：整体进度、当前分区、耗时、结束汇总。 */
function DownloadJobCard({ job, tick }: { job: DownloadJob | null; tick: number }) {
  if (job === null || job.state === 'idle') {
    return <p className="muted">当前没有下载任务。</p>
  }

  const total = job.plan?.total ?? 0
  const overallPct = total > 0 ? (job.done / total) * 100 : 0

  let elapsedSecs: number | null = null
  if (job.started_at !== null) {
    const startMs = new Date(job.started_at).getTime()
    if (job.state === 'running') {
      elapsedSecs = Math.max(0, Math.floor((tick - startMs) / 1000))
    } else if (job.finished_at !== null) {
      elapsedSecs = Math.max(0, Math.floor((new Date(job.finished_at).getTime() - startMs) / 1000))
    }
  }

  return (
    <div className="download-job">
      <div className="download-job-head">
        <span className={STATE_TAG_CLASS[job.state]}>{STATE_LABELS[job.state]}</span>
        {job.request !== null && (
          <span className="muted mono">
            {job.request.symbols.join(', ')} · {job.request.kinds.join(', ')} · {job.request.from} → {job.request.to}
          </span>
        )}
        {elapsedSecs !== null && <span className="muted">已用时长 {duration(elapsedSecs)}</span>}
      </div>

      {job.plan !== null && !job.plan.index_available && (
        <p className="notice notice-warn">未能获取归档范围，已按上个月截止。</p>
      )}

      {job.plan !== null && (
        <div className="progress">
          <div className="progress-bar">
            <div className="progress-fill" style={{ width: `${overallPct}%` }} />
          </div>
          <span className="muted">
            总体进度 {job.done}/{total}
          </span>
        </div>
      )}

      {job.plan !== null && job.plan.clipped.length > 0 && (
        <p className="muted small">
          已按归档范围裁剪：{job.plan.clipped.join('；')}
        </p>
      )}

      {job.current !== null && (
        <div className="download-job-current">
          <p className="muted">
            当前：{job.current.symbol} {job.current.kind} {job.current.month} · {job.current.stage_label}
          </p>
          <div className="progress">
            <div className="progress-bar">
              <div
                className="progress-fill"
                style={{
                  width:
                    job.current.stage_total !== null && job.current.stage_total > 0
                      ? `${(job.current.stage_done / job.current.stage_total) * 100}%`
                      : '100%',
                }}
              />
            </div>
            <span className="muted">
              {job.current.stage_total !== null
                ? `${job.current.stage_done}/${job.current.stage_total}`
                : bytes(job.current.stage_done)}
            </span>
          </div>
        </div>
      )}

      {job.state !== 'running' && (
        <p className="muted">
          成功 {job.completed}、归档无此分区 {job.not_in_archive}
          {job.failed > 0 && `、失败 ${job.failed}`}。
          {job.failed > 0 && ' 失败的分区重跑会重试，已完成与归档无此分区（终态）不会重跑。'}
        </p>
      )}

      {job.failures.length > 0 && (
        <details className="problems">
          <summary>{job.failures.length} 个失败分区</summary>
          <ul>
            {job.failures.map((f, i) => (
              <li key={i} className="mono">
                {f.partition}：{f.error}
              </li>
            ))}
          </ul>
        </details>
      )}

      {job.last_error !== null && (
        <p className="notice notice-error">{job.last_error}</p>
      )}
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
            「归档无此分区」是终态，不会重跑；其余异常分区重跑下载会重新处理。
          </p>
        </details>
      )}
    </div>
  )
}
