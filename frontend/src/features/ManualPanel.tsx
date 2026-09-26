// 手动意图交给后端规划；点击确认后才请求预览，最终提交仍使用同一份意图。
import { useEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { Plus, X } from 'lucide-react'

import { api, newIdempotencyKey } from '../api/client'
import type { InstrumentInfo, ManualPlanRequest, ManualPreview, Side } from '../api/types'
import { InputField } from '../components/FormControls'
import { num, pct, signed } from '../format'
import { useAction } from '../state/store'
import { manualPlan, type RungInput } from './manualPlan'

interface Props {
  instrument: InstrumentInfo
  hasPosition: boolean
  onSubmitted: () => void
  referencePrice?: string | null
}
const DEFAULT_RUNGS: RungInput[] = [
  { bp: '25', price: '', percent: '40' }, { bp: '50', price: '', percent: '30' }, { bp: '75', price: '', percent: '30' },
]
type Review = { plan: ManualPlanRequest; planKey: string; preview?: ManualPreview; error?: string }

export function ManualPanel({ instrument, hasPosition, onSubmitted, referencePrice = null }: Props) {
  const [side, setSide] = useState<Side>('BUY')
  const [entry, setEntry] = useState('')
  const entryInitialized = useRef(false)
  const [stopBp, setStopBp] = useState('25')
  const [stopPrice, setStopPrice] = useState('')
  const [stopMode, setStopMode] = useState<'bp' | 'price'>('bp')
  const [leverage, setLeverage] = useState('3')
  const [sizePct, setSizePct] = useState('10')
  const [rungs, setRungs] = useState<RungInput[]>(DEFAULT_RUNGS)
  const [useLadder, setUseLadder] = useState(true)
  const [singleTpBp, setSingleTpBp] = useState('50')
  const [singleTpPrice, setSingleTpPrice] = useState('')
  const [tpMode, setTpMode] = useState<'bp' | 'price'>('bp')
  const [breakEven, setBreakEven] = useState(true)
  const [cancelSecs, setCancelSecs] = useState('120')
  const [touched, setTouched] = useState<Record<string, boolean>>({})
  const bodyRef = useRef<HTMLFieldSetElement>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)
  const action = useAction()

  // 仅初始化一次；之后行情更新不能悄悄改变待提交价格。
  useEffect(() => {
    if (!entryInitialized.current && referencePrice) {
      entryInitialized.current = true
      setEntry(referencePrice)
    }
  }, [referencePrice])

  const { plan, errors, total } = useMemo(() => manualPlan({
    symbol: instrument.symbol, side, entry, stopBp, stopPrice, stopMode, leverage, sizePct,
    rungs, useLadder, singleTpBp, singleTpPrice, tpMode, breakEven, cancelSecs,
  }), [instrument.symbol, side, entry, stopBp, stopPrice, stopMode, leverage, sizePct, rungs, useLadder, singleTpBp, singleTpPrice, tpMode, breakEven, cancelSecs])
  const planKey = plan === null ? null : JSON.stringify(plan)
  const [review, setReview] = useState<Review | null>(null)
  const [previewRetry, setPreviewRetry] = useState(0)

  useEffect(() => {
    if (review === null) return
    const controller = new AbortController()
    let active = true
    void api.previewManual(review.plan, controller.signal).then((preview) => {
      if (active) setReview((current) => current?.planKey === review.planKey ? { ...current, preview } : current)
    }).catch((error: unknown) => {
      if (active) setReview((current) => current?.planKey === review.planKey
        ? { ...current, error: error instanceof Error ? error.message : String(error) } : current)
    })
    return () => { active = false; controller.abort() }
  }, [review?.planKey, previewRetry])

  const submission = useRef<{ planKey: string; key: string } | null>(null)
  const submitting = useRef(false)
  const [submittedKey, setSubmittedKey] = useState<string | null>(null)
  function openReview() {
    if (hasPosition || action.busy || submittedKey === planKey && planKey !== null) return
    if (plan === null || planKey === null) {
      setTouched(Object.fromEntries(Object.keys(errors).map((key) => [key, true])))
      requestAnimationFrame(() => bodyRef.current?.querySelector<HTMLInputElement>('[aria-invalid="true"]')?.focus())
      return
    }
    action.clear()
    setReview({ plan, planKey })
  }
  function closeReview() {
    if (action.busy) return
    setReview(null)
    requestAnimationFrame(() => triggerRef.current?.focus())
  }
  function retryPreview() {
    setReview((current) => current && { plan: current.plan, planKey: current.planKey })
    setPreviewRetry((value) => value + 1)
  }
  async function submit() {
    if (!review?.preview?.accepted || review.planKey !== planKey || hasPosition || action.busy
      || submittedKey === planKey || submitting.current) return
    submitting.current = true
    if (submission.current?.planKey !== review.planKey) submission.current = { planKey: review.planKey, key: newIdempotencyKey('manual') }
    const key = submission.current.key
    try {
      const response = await action.run(() => api.submitManual(review.plan, key))
      if (response) {
        if (response.accepted) {
          setSubmittedKey(review.planKey)
          setReview(null)
          onSubmitted()
        } else {
          setReview((current) => current?.planKey === review.planKey ? { ...current, preview: response } : current)
        }
      }
    } finally { submitting.current = false }
  }
  const errorFor = (field: string) => touched[field] ? errors[field] : undefined
  const markTouched = (field: string) => () => setTouched((value) => ({ ...value, [field]: true }))
  const status = hasPosition ? '已有持仓或挂单，请先处理后再下新单'
    : submittedKey !== null && submittedKey === planKey ? '已提交，请在当前委托中查看' : null

  return <section className="panel manual-panel" aria-label="手动下单">
    <fieldset ref={bodyRef} className="manual-body" disabled={action.busy}>
      <legend className="sr-only">下单参数</legend>
      <div className="segmented manual-direction" role="group" aria-label="下单方向">
        {(['BUY', 'SELL'] as const).map((value) => <button key={value} type="button"
          className={side === value ? `seg-on ${value === 'BUY' ? 'seg-buy' : 'seg-sell'}` : 'seg'}
          onClick={() => setSide(value)} aria-pressed={side === value}>{value === 'BUY' ? '买入 / 做多' : '卖出 / 做空'}</button>)}
      </div>
      <div className="manual-fields">
        <InputField label="挂单价格" unit={instrument.quote_asset} value={entry} inputMode="decimal"
          placeholder="输入限价" error={errorFor('entry')} onBlur={markTouched('entry')}
          onChange={(value) => { entryInitialized.current = true; setEntry(value) }}
          action={<button type="button" className="input-action" disabled={!referencePrice} onClick={() => {
            if (referencePrice) { entryInitialized.current = true; setEntry(referencePrice) }
          }}>最新价</button>} />
        <div className="manual-grid">
          <InputField label="杠杆" unit="倍" value={leverage} onChange={setLeverage} inputMode="decimal"
            error={errorFor('leverage')} onBlur={markTouched('leverage')} />
          <InputField label="仓位比例" unit="%" value={sizePct} onChange={setSizePct} inputMode="decimal"
            error={errorFor('sizePct')} onBlur={markTouched('sizePct')} />
        </div>
        <div className="size-presets" role="group" aria-label="仓位比例快捷选择">
          {['5', '10', '25', '50'].map((value) => <button key={value} type="button" aria-pressed={sizePct === value}
            onClick={() => setSizePct(value)}>{value}%</button>)}
        </div>
        <div className="manual-mode"><span>止损输入</span><div className="segmented" role="group" aria-label="止损输入方式">
          <button type="button" aria-pressed={stopMode === 'bp'} onClick={() => setStopMode('bp')}>基点</button>
          <button type="button" aria-pressed={stopMode === 'price'} onClick={() => setStopMode('price')}>指定价格</button>
        </div></div>
        <div className="manual-grid">
          {stopMode === 'bp' ? <InputField label="止损距离" unit="bp" value={stopBp} onChange={setStopBp} inputMode="decimal"
            error={errorFor('stopBp')} onBlur={markTouched('stopBp')} />
            : <InputField label="止损价" unit={instrument.quote_asset} value={stopPrice} onChange={setStopPrice} inputMode="decimal"
              error={errorFor('stopPrice')} onBlur={markTouched('stopPrice')} />}
          <InputField label="未成交撤单" unit="秒" value={cancelSecs} onChange={setCancelSecs} inputMode="numeric"
            error={errorFor('cancelSecs')} onBlur={markTouched('cancelSecs')} placeholder="不撤单" />
        </div>
        <div className="manual-tp-choice">
          <span>止盈方式</span>
          <div className="segmented" role="group" aria-label="止盈方式">
            <button type="button" aria-pressed={!useLadder} onClick={() => setUseLadder(false)}>单档</button>
            <button type="button" aria-pressed={useLadder} onClick={() => setUseLadder(true)}>分批</button>
          </div>
        </div>
        <div className="manual-mode"><span>止盈输入</span><div className="segmented" role="group" aria-label="止盈输入方式">
          <button type="button" aria-pressed={tpMode === 'bp'} onClick={() => setTpMode('bp')}>基点</button>
          <button type="button" aria-pressed={tpMode === 'price'} onClick={() => setTpMode('price')}>指定价格</button>
        </div></div>
        {useLadder ? <div className="manual-rungs">
          <div className="rungs-head"><span>档位</span><span>{tpMode === 'bp' ? '距离 / bp' : `目标价 / ${instrument.quote_asset}`}</span><span>平仓 / %</span><span /></div>
          {rungs.map((r, i) => <div className="rung-row" key={i}>
            <span className="rung-index">{i + 1}</span>
            <InputField label={`第 ${i + 1} 档${tpMode === 'bp' ? '止盈距离' : '目标价'}`}
              value={tpMode === 'bp' ? r.bp : r.price} inputMode="decimal" compact
              error={errorFor(`${tpMode}-${i}`)} onBlur={markTouched(`${tpMode}-${i}`)}
              onChange={(value) => setRungs((prev) => prev.map((item, j) => j === i ? { ...item, [tpMode]: value } : item))} />
            <InputField label={`第 ${i + 1} 档平仓比例`} value={r.percent} inputMode="decimal" compact
              error={errorFor(`percent-${i}`)} onBlur={markTouched(`percent-${i}`)}
              onChange={(value) => setRungs((prev) => prev.map((item, j) => j === i ? { ...item, percent: value } : item))} />
            <button type="button" className="icon-btn" aria-label={`删除第 ${i + 1} 档止盈`} disabled={rungs.length <= 1}
              onClick={() => setRungs((prev) => prev.filter((_, j) => i !== j))}><X size={15} aria-hidden="true" /></button>
          </div>)}
          <div className="rungs-foot">
            <span className={errors.rungs ? 'neg' : 'muted'}>合计 {total}%</span>
            <button type="button" className="link-btn" onClick={() => setRungs((prev) => [...prev, { bp: '', price: '', percent: '' }])}>
              <Plus size={14} aria-hidden="true" />增加档位</button>
          </div>
          {errors.rungs && <p className="field-error" role="alert">{errors.rungs}</p>}
        </div> : tpMode === 'bp' ? <InputField label="止盈距离" unit="bp" value={singleTpBp} onChange={setSingleTpBp} inputMode="decimal"
          error={errorFor('singleTpBp')} onBlur={markTouched('singleTpBp')} />
          : <InputField label="止盈目标价" unit={instrument.quote_asset} value={singleTpPrice} onChange={setSingleTpPrice} inputMode="decimal"
            error={errorFor('singleTpPrice')} onBlur={markTouched('singleTpPrice')} />}
        {(stopMode === 'bp' || tpMode === 'bp') && <p className="manual-unit-hint">1 bp = 0.01%，距离以入场价为基准。</p>}
        <label className="checkbox manual-break-even"><input type="checkbox" checked={breakEven} onChange={(e) => setBreakEven(e.target.checked)} />
          <span>保本止损<small>成交档位达到 1R 后移至入场价</small></span></label>
      </div>
    </fieldset>
    <div className="manual-submit">
      {status && <p role="status">{status}</p>}
      {action.error && !review && <p className="field-error" role="alert">{action.error}</p>}
      <button ref={triggerRef} type="button" className={`primary ${side === 'BUY' ? 'buy' : 'sell'}`}
        disabled={hasPosition || action.busy || submittedKey !== null && submittedKey === planKey} onClick={openReview}>
        确认挂单
      </button>
    </div>
    {review && <ReviewDialog review={review} instrument={instrument} busy={action.busy} hasPosition={hasPosition}
      submitError={action.error} onClose={closeReview} onRetry={retryPreview} onSubmit={() => void submit()} />}
  </section>
}

function ReviewDialog({ review, instrument, busy, hasPosition, submitError, onClose, onRetry, onSubmit }: {
  review: Review
  instrument: InstrumentInfo
  busy: boolean
  hasPosition: boolean
  submitError: string | null
  onClose: () => void
  onRetry: () => void
  onSubmit: () => void
}) {
  const dialogRef = useRef<HTMLDialogElement>(null)
  useEffect(() => {
    const dialog = dialogRef.current
    dialog?.showModal()
    return () => { if (dialog?.open) dialog.close() }
  }, [])

  return createPortal(<dialog ref={dialogRef} className="manual-dialog" aria-labelledby="manual-dialog-title"
    onCancel={(event) => { event.preventDefault(); if (!busy) onClose() }}
    onClick={(event) => { if (event.target === event.currentTarget && !busy) onClose() }}>
    <div className="manual-dialog-head">
      <div><h2 id="manual-dialog-title">确认挂单</h2><span>{instrument.symbol} · {review.plan.side === 'BUY' ? '买入 / 做多' : '卖出 / 做空'} · 仅 Maker 限价</span></div>
      <button type="button" className="icon-btn" aria-label="关闭挂单确认" disabled={busy} onClick={onClose}><X size={18} /></button>
    </div>
    <div className="manual-dialog-content">
      <div className="manual-dialog-intent">
        <span>杠杆 {num(review.plan.leverage)} 倍</span>
        {review.plan.size_pct && <span>权益 {pct(review.plan.size_pct, 0)}</span>}
        <span>{review.plan.cancel_unfilled_after_secs ? `${review.plan.cancel_unfilled_after_secs} 秒未成交撤单` : '不自动撤单'}</span>
        <span>保本止损{review.plan.break_even ? '开启' : '关闭'}</span>
      </div>
      {review.preview ? <>
        <div className="manual-dialog-verdict"><span className={review.preview.accepted ? 'tag-ok' : 'tag-bad'}>
          {review.preview.accepted ? '风控通过' : '风控拒绝'}</span>
          <span>价格与数量由后端按合约精度确定</span>
        </div>
        <PreviewBlock preview={review.preview} />
      </> : review.error ? <div className="notice notice-error" role="alert">
        {review.error}<button type="button" className="link-btn" onClick={onRetry}>重新获取预览</button>
      </div> : <p className="manual-dialog-loading" role="status">正在计算挂单价位和风控结果…</p>}
      {hasPosition && <p className="notice notice-warn" role="alert">账户已有持仓或挂单，当前不能再提交新单。</p>}
      {submitError && <p className="notice notice-error" role="alert">{submitError} 请先核对当前委托状态。</p>}
    </div>
    <div className="manual-dialog-actions">
      <button type="button" className="secondary" disabled={busy} onClick={onClose}>{review.preview?.accepted ? '返回修改' : '关闭'}</button>
      <button type="button" className={`primary ${review.plan.side === 'BUY' ? 'buy' : 'sell'}`}
        disabled={!review.preview?.accepted || busy || hasPosition || submitError !== null} onClick={onSubmit}>
        {busy ? '正在提交…' : '提交挂单'}
      </button>
    </div>
  </dialog>, document.body)
}

/** 预览块。展示后端量化后的真实价位。 */
function PreviewBlock({ preview }: { preview: ManualPreview }) {
  return (
    <div className={`preview ${preview.accepted ? '' : 'preview-rejected'}`}>
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
        <div className="table-wrap">
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
        </div>
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
