// 手动下单面板。
//
// # 这是产品的核心理由
//
// 币安没有 OCO、没有 bracket 单，一张条件单只能对应一个数量与触发价。所以
// 「一个点位挂单 + 同时挂分批止盈与止损」必须由客户端实现。这个面板就是
// 那个能力的入口。
//
// # 两条交互约定
//
// 1. **预览必须先于提交。** 用户改任何参数后都重新预览，看到的是后端量化后
//    的真实价位——前端不做任何价格计算。这样「界面显示的价」与「将要挂出的
//    价」必然一致。
// 2. **提交带幂等键。** 双击不会下出两张单。键在用户第一次点击时生成，
//    提交成功后丢弃。

import { useCallback, useEffect, useMemo, useState } from 'react'

import { api, newIdempotencyKey } from '../api/client'
import type { InstrumentInfo, ManualPlanRequest, ManualPreview, Side } from '../api/types'
import { num, pct, signed } from '../format'
import { useAction } from '../state/store'

interface RungInput {
  /** 距入场价的基点。 */
  bp: string
  /** 该档平仓比例（百分比）。 */
  percent: string
}

interface Props {
  instrument: InstrumentInfo
  /** 当前是否已有持仓或在途单。有则禁用下单。 */
  hasPosition: boolean
  onSubmitted: () => void
}

/** 默认三档止盈：25/50/75 bp，比例 40/30/30。 */
const DEFAULT_RUNGS: RungInput[] = [
  { bp: '25', percent: '40' },
  { bp: '50', percent: '30' },
  { bp: '75', percent: '30' },
]

export function ManualPanel({ instrument, hasPosition, onSubmitted }: Props) {
  const [side, setSide] = useState<Side>('BUY')
  const [entry, setEntry] = useState('3200')
  const [stopBp, setStopBp] = useState('25')
  const [leverage, setLeverage] = useState('3')
  const [sizePct, setSizePct] = useState('10')
  const [rungs, setRungs] = useState<RungInput[]>(DEFAULT_RUNGS)
  const [useLadder, setUseLadder] = useState(true)
  const [singleTpBp, setSingleTpBp] = useState('50')
  const [breakEven, setBreakEven] = useState(true)
  const [cancelSecs, setCancelSecs] = useState('120')

  const [preview, setPreview] = useState<ManualPreview | null>(null)
  const action = useAction()

  // 由入场价与基点算出止损价。
  //
  // 这里用定点算术（经 decimal 工具）而非浮点。虽然只是构造请求参数，
  // 但浮点误差会在量化后放大到一整个 tick 的差异。
  const stopPrice = useMemo(() => {
    const e = Number(entry)
    const bp = Number(stopBp)
    if (!Number.isFinite(e) || !Number.isFinite(bp) || e <= 0) return ''
    const delta = (e * bp) / 10_000
    const raw = side === 'BUY' ? e - delta : e + delta
    // 按 tick 对齐到合理精度后交给后端量化
    const tick = Number(instrument.tick_size)
    const aligned = tick > 0 ? Math.round(raw / tick) * tick : raw
    return aligned.toFixed(8).replace(/\.?0+$/, '')
  }, [entry, stopBp, side, instrument.tick_size])

  const buildPlan = useCallback((): ManualPlanRequest | null => {
    if (entry.trim() === '' || stopPrice === '') return null

    const plan: ManualPlanRequest = {
      symbol: instrument.symbol,
      side,
      entry: entry.trim(),
      size_pct: (Number(sizePct) / 100).toString(),
      leverage,
      stop: stopPrice,
      client_ref: 'manual',
    }
    if (cancelSecs.trim() !== '') {
      plan.cancel_unfilled_after_secs = Number(cancelSecs)
    }
    if (breakEven) {
      plan.break_even = { trigger_r: '1', offset: '0' }
    }
    if (useLadder) {
      plan.take_profit = rungs
        .filter((r) => r.bp.trim() !== '' && r.percent.trim() !== '')
        .map((r) => ({
          pct: (Number(r.bp) / 10_000).toString(),
          fraction: (Number(r.percent) / 100).toString(),
        }))
    } else {
      plan.take_profit_pct = (Number(singleTpBp) / 10_000).toString()
    }
    return plan
  }, [
    instrument.symbol,
    side,
    entry,
    stopPrice,
    sizePct,
    leverage,
    cancelSecs,
    breakEven,
    useLadder,
    rungs,
    singleTpBp,
  ])

  // 参数变化后自动重新预览。
  //
  // 防抖 300ms：用户连续输入时不必每次都发请求。预览是只读操作，不会
  // 改变引擎状态，所以频繁调用是安全的。
  useEffect(() => {
    const plan = buildPlan()
    if (plan === null) {
      setPreview(null)
      return
    }
    const timer = setTimeout(() => {
      void action.run(async () => {
        const p = await api.previewManual(plan)
        setPreview(p)
        return p
      })
    }, 300)
    return () => clearTimeout(timer)
    // action.run 是稳定的 useCallback，不需要进依赖
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [buildPlan])

  const submit = useCallback(async () => {
    const plan = buildPlan()
    if (plan === null) return
    // 幂等键在此生成：同一次点击的重复提交会因键重复被后端拒绝，
    // 而不是下出两张单。
    const key = newIdempotencyKey('manual')
    const result = await action.run(() => api.submitManual(plan, key))
    if (result !== undefined) {
      onSubmitted()
    }
  }, [buildPlan, action, onSubmitted])

  // 分批比例之和校验。超过 100% 会超卖，必须在前端就拦住并说明。
  const rungPercentTotal = rungs.reduce((sum, r) => {
    const v = Number(r.percent)
    return sum + (Number.isFinite(v) ? v : 0)
  }, 0)
  const rungTotalOk = !useLadder || rungPercentTotal <= 100

  const disabled = hasPosition || action.busy

  return (
    <section className="panel" aria-labelledby="manual-title">
      <h2 id="manual-title">手动下单</h2>

      {hasPosition && (
        <p className="notice notice-warn">
          当前已有持仓或在途订单。要下新单请先平仓或撤掉在途单。
        </p>
      )}

      <div className="row">
        <label>
          <span>方向</span>
          <div className="segmented" role="group" aria-label="下单方向">
            <button
              type="button"
              className={side === 'BUY' ? 'seg-on seg-buy' : 'seg'}
              onClick={() => setSide('BUY')}
              aria-pressed={side === 'BUY'}
            >
              做多
            </button>
            <button
              type="button"
              className={side === 'SELL' ? 'seg-on seg-sell' : 'seg'}
              onClick={() => setSide('SELL')}
              aria-pressed={side === 'SELL'}
            >
              做空
            </button>
          </div>
        </label>
      </div>

      <div className="row">
        <Field
          label="挂单价格"
          unit={instrument.quote_asset}
          value={entry}
          onChange={setEntry}
          hint="限价单（GTX），只会作为 maker 成交"
        />
        <Field
          label="止损距离"
          unit="基点"
          value={stopBp}
          onChange={setStopBp}
          hint={`突破即认错。止损价 ${stopPrice || '—'}`}
        />
      </div>

      <div className="row">
        <Field
          label="杠杆"
          unit="倍"
          value={leverage}
          onChange={setLeverage}
          hint={`维持保证金率 ${instrument.maint_margin_pct}%`}
        />
        <Field
          label="仓位比例"
          unit="%"
          value={sizePct}
          onChange={setSizePct}
          hint="占可用权益的百分比"
        />
      </div>

      <div className="row">
        <label className="checkbox">
          <input
            type="checkbox"
            checked={useLadder}
            onChange={(e) => setUseLadder(e.target.checked)}
          />
          <span>分批止盈</span>
        </label>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={breakEven}
            onChange={(e) => setBreakEven(e.target.checked)}
          />
          <span>保本止损（浮盈 1 倍止损距离后推到入场价）</span>
        </label>
      </div>

      {useLadder ? (
        <div className="rungs">
          <div className="rungs-head">
            <span>档位</span>
            <span>距入场</span>
            <span>平仓比例</span>
            <span />
          </div>
          {rungs.map((r, i) => (
            <div className="rung-row" key={i}>
              <span className="rung-index">第 {i + 1} 档</span>
              <Field
                label=""
                unit="基点"
                value={r.bp}
                onChange={(v) =>
                  setRungs((prev) => prev.map((x, j) => (j === i ? { ...x, bp: v } : x)))
                }
                compact
              />
              <Field
                label=""
                unit="%"
                value={r.percent}
                onChange={(v) =>
                  setRungs((prev) =>
                    prev.map((x, j) => (j === i ? { ...x, percent: v } : x)),
                  )
                }
                compact
              />
              <button
                type="button"
                className="icon-btn"
                aria-label={`删除第 ${i + 1} 档止盈`}
                onClick={() => setRungs((prev) => prev.filter((_, j) => j !== i))}
                disabled={rungs.length <= 1}
              >
                ×
              </button>
            </div>
          ))}
          <div className="rungs-foot">
            <span>合计平仓 {rungPercentTotal}%</span>
            <button
              type="button"
              className="link-btn"
              onClick={() => setRungs((prev) => [...prev, { bp: '100', percent: '0' }])}
            >
              + 增加档位
            </button>
          </div>
          {!rungTotalOk && (
            <p className="notice notice-error">
              各档平仓比例合计 {rungPercentTotal}% 超过 100%，会被后端拒绝。
            </p>
          )}
        </div>
      ) : (
        <div className="row">
          <Field
            label="止盈距离"
            unit="基点"
            value={singleTpBp}
            onChange={setSingleTpBp}
            hint="单一止盈价，全额平仓"
          />
        </div>
      )}

      <div className="row">
        <Field
          label="挂单自动撤销"
          unit="秒"
          value={cancelSecs}
          onChange={setCancelSecs}
          hint="超过此时间未成交则撤单"
        />
      </div>

      {preview !== null && <PreviewBlock preview={preview} />}

      {action.error !== null && (
        <p className="notice notice-error" role="alert">
          {action.error}
        </p>
      )}

      <div className="row actions">
        <button
          type="button"
          className={`primary ${side === 'BUY' ? 'buy' : 'sell'}`}
          onClick={() => void submit()}
          disabled={disabled || preview === null || !preview.accepted || !rungTotalOk}
        >
          {action.busy ? '提交中…' : side === 'BUY' ? '挂买单' : '挂卖单'}
        </button>
      </div>
    </section>
  )
}

/** 预览块。展示后端量化后的真实价位。 */
function PreviewBlock({ preview }: { preview: ManualPreview }) {
  return (
    <div className={`preview ${preview.accepted ? '' : 'preview-rejected'}`}>
      <div className="preview-head">
        <span>下单预览</span>
        <span className={preview.accepted ? 'tag-ok' : 'tag-bad'}>
          {preview.accepted ? '风控通过' : '风控拒绝'}
        </span>
      </div>

      {preview.reject_reason !== null && (
        <p className="notice notice-error" role="alert">
          {preview.reject_reason}
        </p>
      )}

      <dl className="kv">
        <div>
          <dt>入场价</dt>
          <dd>{num(preview.entry)}</dd>
        </div>
        <div>
          <dt>止损价</dt>
          <dd className="neg">{num(preview.stop)}</dd>
        </div>
        <div>
          <dt>数量</dt>
          <dd>{num(preview.quantity)}</dd>
        </div>
        <div>
          <dt>名义价值</dt>
          <dd>{num(preview.notional)}</dd>
        </div>
        <div>
          <dt>保证金占用</dt>
          <dd>{num(preview.margin_required, 2)}</dd>
        </div>
        {preview.liquidation_buffer_pct !== null && (
          <div>
            <dt>止损距强平</dt>
            <dd>{pct(preview.liquidation_buffer_pct, 3)}</dd>
          </div>
        )}
      </dl>

      {preview.take_profits.length > 0 && (
        <table className="mini-table">
          <caption className="sr-only">分批止盈明细</caption>
          <thead>
            <tr>
              <th scope="col">档位</th>
              <th scope="col">价格</th>
              <th scope="col">数量</th>
              <th scope="col">距入场</th>
              <th scope="col">毛利</th>
            </tr>
          </thead>
          <tbody>
            {preview.take_profits.map((r) => (
              <tr key={r.rung}>
                <td>第 {r.index} 档</td>
                <td>{num(r.price)}</td>
                <td>{num(r.quantity)}</td>
                <td>{num(r.distance_bp, 2)} bp</td>
                <td className="pos">{signed(r.gross_profit)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {preview.warnings.length > 0 && (
        <ul className="warnings">
          {preview.warnings.map((w, i) => (
            <li key={i}>{w}</li>
          ))}
        </ul>
      )}
    </div>
  )
}

/**
 * 数值输入框。
 *
 * 用 `type="text"` 而非 `type="number"`：数字输入框在部分浏览器上会对小数
 * 做本地化处理（逗号小数点），而我们的价格必须原样传给后端。
 */
function Field({
  label,
  unit,
  value,
  onChange,
  hint,
  compact = false,
}: {
  label: string
  unit: string
  value: string
  onChange: (v: string) => void
  hint?: string
  compact?: boolean
}) {
  const id = `f-${label}-${unit}-${Math.random().toString(36).slice(2, 7)}`
  return (
    <div className={compact ? 'field field-compact' : 'field'}>
      {label !== '' && <label htmlFor={id}>{label}</label>}
      <div className="input-wrap">
        <input
          id={id}
          type="text"
          inputMode="decimal"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          aria-label={label !== '' ? undefined : unit}
        />
        {unit !== '' && <span className="unit">{unit}</span>}
      </div>
      {hint !== undefined && <small>{hint}</small>}
    </div>
  )
}
