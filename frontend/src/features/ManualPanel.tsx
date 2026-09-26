// 手动意图交给后端规划；只有与当前输入完全一致的预览可以提交。
import { useEffect, useMemo, useRef, useState } from 'react'
import { SlidersHorizontal, Plus, X } from 'lucide-react'

import { api, newIdempotencyKey } from '../api/client'
import type { InstrumentInfo, ManualPreview, Side } from '../api/types'
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
  { bp: '25', percent: '40' }, { bp: '50', percent: '30' }, { bp: '75', percent: '30' },
]

export function ManualPanel({ instrument, hasPosition, onSubmitted, referencePrice = null }: Props) {
  const [side, setSide] = useState<Side>('BUY')
  const [entry, setEntry] = useState('')
  const entryInitialized = useRef(false)
  const [stopBp, setStopBp] = useState('25')
  const [leverage, setLeverage] = useState('3')
  const [sizePct, setSizePct] = useState('10')
  const [rungs, setRungs] = useState<RungInput[]>(DEFAULT_RUNGS)
  const [useLadder, setUseLadder] = useState(true)
  const [singleTpBp, setSingleTpBp] = useState('50')
  const [breakEven, setBreakEven] = useState(true)
  const [cancelSecs, setCancelSecs] = useState('120')
  const [touched, setTouched] = useState<Record<string, boolean>>({})
  const action = useAction()

  // 仅初始化一次；之后行情更新不能悄悄改变待提交价格。
  useEffect(() => {
    if (!entryInitialized.current && referencePrice) {
      entryInitialized.current = true
      setEntry(referencePrice)
    }
  }, [referencePrice])

  const { plan, errors, total } = useMemo(() => manualPlan({
    symbol: instrument.symbol, side, entry, stopBp, leverage, sizePct,
    rungs, useLadder, singleTpBp, breakEven, cancelSecs,
  }), [instrument.symbol, side, entry, stopBp, leverage, sizePct, rungs, useLadder, singleTpBp, breakEven, cancelSecs])
  const planKey = plan === null ? null : JSON.stringify(plan)
  const [result, setResult] = useState<{ key: string; preview?: ManualPreview; error?: string } | null>(null)
  const [retry, setRetry] = useState(0)
  const current = result?.key === planKey ? result : null
  const preview = current?.preview ?? null
  const previewing = planKey !== null && current === null

  useEffect(() => {
    if (planKey === null) return
    const controller = new AbortController()
    let active = true
    const timer = setTimeout(() => {
      void api.previewManual(JSON.parse(planKey), controller.signal).then((preview) => {
        if (active) setResult({ key: planKey, preview })
      }).catch((error: unknown) => {
        if (active) setResult({ key: planKey, error: error instanceof Error ? error.message : String(error) })
      })
    }, 300)
    return () => { active = false; clearTimeout(timer); controller.abort() }
  }, [planKey, retry])

  const submission = useRef<{ planKey: string; key: string } | null>(null)
  const submitting = useRef(false)
  const [submittedKey, setSubmittedKey] = useState<string | null>(null)
  const canSubmit = !hasPosition && !action.busy && plan !== null && preview?.accepted === true && submittedKey !== planKey
  async function submit() {
    if (!canSubmit || !plan || !planKey || submitting.current) return
    submitting.current = true
    if (submission.current?.planKey !== planKey) submission.current = { planKey, key: newIdempotencyKey('manual') }
    const key = submission.current.key
    try {
      const response = await action.run(() => api.submitManual(plan, key))
      if (response) {
        setResult({ key: planKey, preview: response })
        if (response.accepted) { setSubmittedKey(planKey); onSubmitted() }
      }
    } finally { submitting.current = false }
  }
  const errorFor = (field: string) => touched[field] ? errors[field] : undefined
  const markTouched = (field: string) => () => setTouched((value) => ({ ...value, [field]: true }))
  const status = hasPosition ? '已有持仓或挂单，请先处理后再下新单'
    : submittedKey !== null && submittedKey === planKey ? '已提交，请在当前委托中查看'
    : plan === null ? '填写有效参数后自动生成下单预览'
    : previewing ? '正在更新预览…'
    : current?.error ? '预览失败，请重试'
    : preview?.accepted ? '当前参数已通过风控检查' : '请根据预览提示调整参数'

  return <section className="panel manual-panel" aria-labelledby="manual-title">
    <div className="panel-head">
      <h2 id="manual-title"><SlidersHorizontal size={17} aria-hidden="true" />手动下单</h2>
      <span className="tag">仅 Maker</span>
    </div>
    <fieldset className="manual-body" disabled={action.busy}>
      <legend className="sr-only">下单参数</legend>
      <div className="segmented manual-direction" role="group" aria-label="下单方向">
        {(['BUY', 'SELL'] as const).map((value) => <button key={value} type="button"
          className={side === value ? `seg-on ${value === 'BUY' ? 'seg-buy' : 'seg-sell'}` : 'seg'}
          onClick={() => setSide(value)} aria-pressed={side === value}>{value === 'BUY' ? '买入 / 做多' : '卖出 / 做空'}</button>)}
      </div>
      <section className="manual-section" aria-labelledby="entry-section">
        <h3 id="entry-section"><span>01</span> 入场与仓位</h3>
        <InputField label="挂单价格" unit={instrument.quote_asset} value={entry} inputMode="decimal"
          placeholder="输入限价" error={errorFor('entry')} onBlur={markTouched('entry')}
          onChange={(value) => { entryInitialized.current = true; setEntry(value) }}
          action={<button type="button" className="input-action" disabled={!referencePrice} onClick={() => {
            if (referencePrice) { entryInitialized.current = true; setEntry(referencePrice) }
          }}>最新价</button>}
          hint="仅挂限价单，实际价位以下方预览为准" />
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
        <p className="field-hint">仓位按可用权益计算 · 维持保证金率 {instrument.maint_margin_pct}%</p>
      </section>
      <section className="manual-section" aria-labelledby="protection-section">
        <h3 id="protection-section"><span>02</span> 止盈与止损</h3>
        <InputField label="止损距离" unit="基点" value={stopBp} onChange={setStopBp} inputMode="decimal"
          error={errorFor('stopBp')} onBlur={markTouched('stopBp')}
          hint={`1 基点 = 0.01% · 止损价 ${preview ? num(preview.stop) : '等待预览'}`} />
        <div className="segmented" role="group" aria-label="止盈方式">
          <button type="button" aria-pressed={!useLadder} onClick={() => setUseLadder(false)}>单档止盈</button>
          <button type="button" aria-pressed={useLadder} onClick={() => setUseLadder(true)}>分批止盈</button>
        </div>
        {useLadder ? <div className="manual-rungs">
          <div className="rungs-head"><span>档位</span><span>距离 / bp</span><span>平仓 / %</span><span /></div>
          {rungs.map((r, i) => <div className="rung-row" key={i}>
            <span className="rung-index">{i + 1}</span>
            <InputField label={`第 ${i + 1} 档止盈距离`} value={r.bp} inputMode="decimal" compact
              error={errorFor(`bp-${i}`)} onBlur={markTouched(`bp-${i}`)}
              onChange={(value) => setRungs((prev) => prev.map((item, j) => j === i ? { ...item, bp: value } : item))} />
            <InputField label={`第 ${i + 1} 档平仓比例`} value={r.percent} inputMode="decimal" compact
              error={errorFor(`percent-${i}`)} onBlur={markTouched(`percent-${i}`)}
              onChange={(value) => setRungs((prev) => prev.map((item, j) => j === i ? { ...item, percent: value } : item))} />
            <button type="button" className="icon-btn" aria-label={`删除第 ${i + 1} 档止盈`} disabled={rungs.length <= 1}
              onClick={() => setRungs((prev) => prev.filter((_, j) => i !== j))}><X size={15} aria-hidden="true" /></button>
          </div>)}
          <div className="rungs-foot">
            <span className={errors.rungs ? 'neg' : 'muted'}>合计 {total}%</span>
            <button type="button" className="link-btn" onClick={() => setRungs((prev) => [...prev, { bp: '', percent: '' }])}>
              <Plus size={14} aria-hidden="true" />增加档位</button>
          </div>
          {errors.rungs && <p className="field-error" role="alert">{errors.rungs}</p>}
          <p className="field-hint">各档比例均以入场时的原始持仓量为基准。</p>
        </div> : <InputField label="止盈距离" unit="基点" value={singleTpBp} onChange={setSingleTpBp} inputMode="decimal"
          error={errorFor('singleTpBp')} onBlur={markTouched('singleTpBp')} hint="单档止盈，全额平仓" />}
        <label className="checkbox manual-break-even"><input type="checkbox" checked={breakEven} onChange={(e) => setBreakEven(e.target.checked)} />
          <span>保本止损<small>浮盈达到 1 倍止损距离后，止损推至入场价</small></span></label>
        <InputField label="未成交自动撤单" unit="秒" value={cancelSecs} onChange={setCancelSecs} inputMode="numeric"
          error={errorFor('cancelSecs')} onBlur={markTouched('cancelSecs')} hint="留空则不自动撤销" />
      </section>
      <section className="manual-section manual-review" aria-labelledby="review-section">
        <h3 id="review-section"><span>03</span> 确认下单</h3>
        {preview ? <PreviewBlock preview={preview} /> : <p className="manual-preview-status" role="status">{status}</p>}
        {current?.error && <div className="notice notice-error" role="alert">{current.error}
          <button type="button" className="link-btn" onClick={() => { setResult(null); setRetry((value) => value + 1) }}>重新预览</button>
        </div>}
      </section>
    </fieldset>
    <div className="manual-submit">
      <p role="status">{status}</p>
      {preview && <div className="manual-submit-summary">
        <span>数量 <strong>{num(preview.quantity)} {instrument.base_asset}</strong></span>
        <span>保证金 <strong>{num(preview.margin_required, 2)} {instrument.margin_asset}</strong></span>
      </div>}
      {action.error && <p className="field-error" role="alert">{action.error}</p>}
      <button type="button" className={`primary ${side === 'BUY' ? 'buy' : 'sell'}`} disabled={!canSubmit} onClick={() => void submit()}>
        {action.busy ? '正在提交…' : side === 'BUY' ? '确认挂买单' : '确认挂卖单'}
      </button>
    </div>
  </section>
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
