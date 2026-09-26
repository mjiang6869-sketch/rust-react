// 自动化做市（区间做市策略）面板：开关 + 参数 + 运行状态。
//
// # 两条独立的数据来源
//
// - 参数的合法范围与说明（`fields`）、以及初次加载的参数基线来自 REST
//   `GET /api/v1/auto-maker`——只在挂载与手动重试时拉一次。
// - 运行状态（`enabled`/`status`/`status_label`/预热进度）来自 WebSocket
//   推送的 `engine.auto_maker`，与手动下单面板读取持仓的方式一致，
//   刷新更及时。参数基线也会被 ws 推送覆盖（另一个标签页改了参数时），
//   见下面「外部参数变化」的处理。
//
// # 关闭状态下也能编辑
//
// 用户经常是先调好参数再启用，而不是启用后再调——所以草稿编辑不依赖
// `enabled`，只在提交网络请求时禁用输入框（`action.busy`）。

import { useEffect, useMemo, useState } from 'react'
import { Bot, RotateCcw } from 'lucide-react'

import { ApiError, api } from '../api/client'
import { cmpStr, parse, toPercent } from '../api/decimal.ts'
import type { AutoMakerParams, EngineState, ParameterInfo } from '../api/types'
import { InputField } from '../components/FormControls'
import { useAction } from '../state/store'
import {
  type AutoMakerDraft,
  type NumericKey,
  buildParams,
  draftFromDefaults,
  draftFromParams,
  isDraftDirty,
} from './autoMakerForm.ts'

export interface AutoMakerPanelProps {
  engine: EngineState
  /** 当前页面正在看的交易对；引擎实际跑的交易对见 `engine.symbol`，两者可能不同。 */
  symbol: string
}

function sameParams(a: AutoMakerParams, b: AutoMakerParams): boolean {
  return (
    a.lookback === b.lookback &&
    a.take_profit_bp === b.take_profit_bp &&
    a.stop_buffer_bp === b.stop_buffer_bp &&
    a.side_mode === b.side_mode &&
    a.equity_pct === b.equity_pct &&
    a.leverage === b.leverage &&
    a.valid_minutes === b.valid_minutes
  )
}

/** 参数说明的范围提示。lookback 的上限还受 `max_lookback`（引擎实际能回看的根数）约束。 */
function rangeHint(field: ParameterInfo, maxLookback: number): string {
  const effectiveMax =
    field.key === 'lookback' && cmpStr(String(maxLookback), field.max) < 0 ? String(maxLookback) : field.max
  if (field.display_as_percent) {
    return `范围 ${toPercent(parse(field.min), 6)}%–${toPercent(parse(effectiveMax), 6)}%`
  }
  const unit = field.unit ? ` ${field.unit}` : ''
  return `范围 ${field.min}–${effectiveMax}${unit}`
}

export function AutoMakerPanel({ engine, symbol }: AutoMakerPanelProps) {
  const action = useAction()
  const auto = engine.auto_maker

  const [fields, setFields] = useState<ParameterInfo[] | null>(null)
  const [baseline, setBaseline] = useState<AutoMakerParams | null>(null)
  const [draft, setDraft] = useState<AutoMakerDraft | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [touched, setTouched] = useState<Record<string, boolean>>({})
  const [externalNotice, setExternalNotice] = useState(false)
  const [reloadKey, setReloadKey] = useState(0)

  // 初次加载（与手动重试）。
  useEffect(() => {
    let active = true
    setLoadError(null)
    api
      .autoMaker()
      .then((cfg) => {
        if (!active) return
        setFields(cfg.fields)
        setBaseline(cfg.params)
        setDraft(draftFromParams(cfg.params, cfg.fields))
      })
      .catch((e: unknown) => {
        if (!active) return
        setLoadError(e instanceof ApiError ? e.message : `加载失败：${e instanceof Error ? e.message : String(e)}`)
      })
    return () => {
      active = false
    }
  }, [reloadKey])

  // 外部参数变化：另一个标签页（或本次操作之外的途径）改了参数。
  // 草稿未改动就跟着同步；已改动就保留草稿，只提示一句。
  useEffect(() => {
    if (fields === null || baseline === null || draft === null) return
    if (sameParams(auto.params, baseline)) return
    const dirty = isDraftDirty(draft, baseline, fields)
    setBaseline(auto.params)
    if (dirty) setExternalNotice(true)
    else setDraft(draftFromParams(auto.params, fields))
  }, [auto.params, fields, baseline, draft])

  const built = useMemo(() => {
    if (fields === null || draft === null) return { params: null as AutoMakerParams | null, errors: {} as Record<string, string> }
    return buildParams(draft, fields)
  }, [draft, fields])

  const dirty = fields !== null && baseline !== null && draft !== null && isDraftDirty(draft, baseline, fields)

  function applyResponse(cfg: { fields: ParameterInfo[]; params: AutoMakerParams }) {
    setFields(cfg.fields)
    setBaseline(cfg.params)
    setDraft(draftFromParams(cfg.params, cfg.fields))
    setExternalNotice(false)
    setTouched({})
  }

  async function enable() {
    if (built.params === null) return
    const response = await action.run(() => api.setAutoMaker({ enabled: true, params: built.params! }))
    if (response) applyResponse(response)
  }
  async function disable() {
    const response = await action.run(() => api.setAutoMaker({ enabled: false }))
    if (response) applyResponse(response)
  }
  async function applyDraft() {
    if (built.params === null) return
    const response = await action.run(() => api.setAutoMaker({ enabled: true, params: built.params! }))
    if (response) applyResponse(response)
  }
  function restoreDefaults() {
    if (fields === null) return
    setDraft(draftFromDefaults(fields))
    setExternalNotice(false)
  }
  function setFieldValue(key: NumericKey, value: string) {
    setDraft((prev) => (prev === null ? prev : { ...prev, [key]: value }))
    setExternalNotice(false)
  }
  function setSideMode(mode: AutoMakerParams['side_mode']) {
    setDraft((prev) => (prev === null ? prev : { ...prev, side_mode: mode }))
    setExternalNotice(false)
  }
  const errorFor = (key: string) => (touched[key] ? built.errors[key] : undefined)
  const markTouched = (key: string) => () => setTouched((prev) => ({ ...prev, [key]: true }))

  if (loadError !== null && fields === null) {
    return (
      <section className="panel auto-maker-panel" aria-labelledby="auto-maker-title">
        <div className="panel-head">
          <h2 id="auto-maker-title">
            <Bot size={17} aria-hidden="true" />自动化做市
          </h2>
        </div>
        <div className="notice notice-error" role="alert">
          {loadError}
          <button type="button" className="link-btn" onClick={() => setReloadKey((v) => v + 1)}>
            重试
          </button>
        </div>
      </section>
    )
  }

  if (fields === null || draft === null) {
    return (
      <section className="panel auto-maker-panel" aria-labelledby="auto-maker-title">
        <div className="panel-head">
          <h2 id="auto-maker-title">
            <Bot size={17} aria-hidden="true" />自动化做市
          </h2>
        </div>
        <p className="panel-empty">正在加载配置…</p>
      </section>
    )
  }

  const warmupPct =
    auto.status === 'WARMING_UP' ? Math.min(100, Math.round((auto.warmup_have / Math.max(1, auto.warmup_need)) * 100)) : 0

  return (
    <section className="panel auto-maker-panel" aria-labelledby="auto-maker-title">
      <div className="panel-head">
        <h2 id="auto-maker-title">
          <Bot size={17} aria-hidden="true" />自动化做市
        </h2>
        <span className={auto.enabled ? 'tag-ok' : 'tag'}>{auto.enabled ? '运行中' : '未启用'}</span>
        <span className="head-note">{auto.strategy_name}</span>
      </div>

      <div className="auto-maker-status">
        <p className="auto-maker-status-label">{auto.status_label}</p>
        {auto.status === 'WARMING_UP' && (
          <div className="auto-maker-warmup">
            <div className="auto-maker-warmup-bar" role="img" aria-label={`预热进度 ${auto.warmup_have}/${auto.warmup_need}`}>
              <span style={{ width: `${warmupPct}%` }} />
            </div>
            <span className="muted">
              预热 {auto.warmup_have} / {auto.warmup_need} 根 K 线
            </span>
          </div>
        )}
        {!auto.enabled && engine.position_source === 'STRATEGY' && (
          <p className="notice notice-info">策略持仓仍受止盈止损保护，可在持仓卡片手动平仓。</p>
        )}
        {symbol !== engine.symbol && <p className="auto-maker-elsewhere muted">运行在 {engine.symbol}</p>}
      </div>

      <fieldset className="auto-maker-body" disabled={action.busy}>
        <legend className="sr-only">自动化做市参数</legend>
        <div className="segmented auto-maker-side" role="group" aria-label="做市方向">
          {(['LONG_ONLY', 'SHORT_ONLY'] as const).map((mode) => (
            <button key={mode} type="button" aria-pressed={draft.side_mode === mode} onClick={() => setSideMode(mode)}>
              {mode === 'LONG_ONLY' ? '只做多' : '只做空'}
            </button>
          ))}
        </div>

        {fields.map((field) => (
          <InputField
            key={field.key}
            label={field.label}
            unit={field.display_as_percent ? '%' : field.unit ?? ''}
            value={draft[field.key as NumericKey]}
            inputMode="decimal"
            error={errorFor(field.key)}
            onBlur={markTouched(field.key)}
            onChange={(value) => setFieldValue(field.key as NumericKey, value)}
            hint={rangeHint(field, auto.max_lookback)}
          />
        ))}

        <p className="field-hint auto-maker-note">保本 / 移动止损：模拟盘引擎暂不执行，暂不开放。</p>
      </fieldset>

      {action.error !== null && (
        <p className="field-error" role="alert">
          {action.error}
        </p>
      )}
      {externalNotice && <p className="notice notice-info">参数已在别处更新。</p>}

      <div className="actions auto-maker-actions">
        {auto.enabled ? (
          <>
            <button type="button" className="danger" disabled={action.busy} onClick={() => void disable()}>
              {action.busy ? '处理中…' : '停用'}
            </button>
            <button
              type="button"
              className="secondary"
              disabled={action.busy || !dirty || built.params === null}
              onClick={() => void applyDraft()}
            >
              应用参数
            </button>
          </>
        ) : (
          <button type="button" className="primary" disabled={action.busy || built.params === null} onClick={() => void enable()}>
            {action.busy ? '处理中…' : '启用自动化做市'}
          </button>
        )}
        <button type="button" className="link-btn" disabled={action.busy} onClick={restoreDefaults}>
          <RotateCcw size={14} aria-hidden="true" />恢复默认
        </button>
      </div>
    </section>
  )
}
