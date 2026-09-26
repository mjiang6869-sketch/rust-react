// 行情页顶部的自动化做市开关与参数弹层。
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

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { Bot, RotateCcw, Settings2, X } from 'lucide-react'

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
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [settingsPlacement, setSettingsPlacement] = useState<{ top: number; left: number; width: number; maxHeight: number } | null>(null)
  const controlRef = useRef<HTMLDivElement>(null)
  const settingsButtonRef = useRef<HTMLButtonElement>(null)
  const settingsPanelRef = useRef<HTMLElement>(null)
  const recentSave = useRef<{ params: AutoMakerParams; at: number } | null>(null)

  useLayoutEffect(() => {
    if (!settingsOpen) return
    const positionSettings = () => {
      const trigger = settingsButtonRef.current?.getBoundingClientRect()
      if (!trigger) return
      const width = Math.min(420, window.innerWidth - 24)
      const left = Math.max(8, Math.min(trigger.right - width, window.innerWidth - width - 8))
      setSettingsPlacement({ top: trigger.bottom + 8, left, width, maxHeight: Math.max(0, Math.min(720, window.innerHeight - trigger.bottom - 16)) })
    }
    positionSettings()
    const content = controlRef.current?.closest('.content')
    const app = controlRef.current?.closest('.app')
    window.addEventListener('resize', positionSettings)
    content?.addEventListener('scroll', positionSettings)
    app?.addEventListener('scroll', positionSettings)
    return () => {
      window.removeEventListener('resize', positionSettings)
      content?.removeEventListener('scroll', positionSettings)
      app?.removeEventListener('scroll', positionSettings)
    }
  }, [settingsOpen])

  useEffect(() => {
    if (settingsOpen) settingsPanelRef.current?.focus({ preventScroll: true })
  }, [settingsOpen])

  useEffect(() => {
    if (!settingsOpen) return
    const closeOnOutside = (event: PointerEvent) => {
      if (!controlRef.current?.contains(event.target as Node)) setSettingsOpen(false)
    }
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      setSettingsOpen(false)
      settingsButtonRef.current?.focus({ preventScroll: true })
    }
    document.addEventListener('pointerdown', closeOnOutside)
    document.addEventListener('keydown', closeOnEscape)
    return () => {
      document.removeEventListener('pointerdown', closeOnOutside)
      document.removeEventListener('keydown', closeOnEscape)
    }
  }, [settingsOpen])

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
    // PUT 的成功响应可能早于下一帧 WebSocket 快照；别让旧快照撤销刚保存的参数。
    if (recentSave.current !== null) {
      if (sameParams(auto.params, recentSave.current.params)) recentSave.current = null
      else if (Date.now() - recentSave.current.at < 5_000) return
      else recentSave.current = null
    }
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
    recentSave.current = { params: cfg.params, at: Date.now() }
    setFields(cfg.fields)
    setBaseline(cfg.params)
    setDraft(draftFromParams(cfg.params, cfg.fields))
    setExternalNotice(false)
    setTouched({})
  }

  async function toggle() {
    // 顶部开关只切换运行状态，不把弹层中尚未保存的草稿一并提交。
    await action.run(() => api.setAutoMaker({ enabled: !auto.enabled }))
  }
  async function applyDraft() {
    const params = built.params
    if (params === null) return
    // 后端支持在关闭状态下保存参数；保存不能悄悄启用策略。
    const response = await action.run(() => api.setAutoMaker({ enabled: auto.enabled, params }))
    if (response) {
      applyResponse(response)
      setSettingsOpen(false)
      settingsButtonRef.current?.focus({ preventScroll: true })
    }
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

  const warmupPct =
    auto.status === 'WARMING_UP' ? Math.min(100, Math.round((auto.warmup_have / Math.max(1, auto.warmup_need)) * 100)) : 0

  return (
    <div className="auto-maker-control" ref={controlRef}>
      <div className="auto-maker-control-main">
        <span className="auto-maker-control-label"><Bot size={16} aria-hidden="true" />自动做市</span>
        <span className={auto.enabled ? 'tag-ok' : 'tag'}>{auto.enabled ? '已开启' : '已关闭'}</span>
        {auto.enabled && <span className="auto-maker-control-detail">{auto.status_label}</span>}
        {symbol !== engine.symbol && <span className="auto-maker-control-detail">运行于 {engine.symbol}</span>}
        {!auto.enabled && engine.position_source === 'STRATEGY' && <span className="auto-maker-control-detail warn">策略持仓仍在</span>}
        {!auto.enabled && engine.mode === 'LIVE' && <span className="auto-maker-control-detail warn">仅模拟盘可开启</span>}
        <button type="button" className={auto.enabled ? 'secondary' : 'primary'}
          disabled={action.busy || (!auto.enabled && engine.mode === 'LIVE')}
          title={!auto.enabled && engine.mode === 'LIVE' ? '仅模拟盘可开启自动化做市' : undefined}
          onClick={() => void toggle()}>
          {action.busy ? '处理中…' : auto.enabled ? '关闭做市' : '开启做市'}
        </button>
        <button ref={settingsButtonRef} type="button" className="secondary auto-maker-settings-button"
          aria-expanded={settingsOpen} aria-controls="auto-maker-settings"
          onClick={() => setSettingsOpen((open) => !open)}>
          <Settings2 size={15} aria-hidden="true" />参数{dirty && <span className="auto-maker-dirty" aria-label="有未保存参数" />}
        </button>
      </div>
      {action.error !== null && !settingsOpen && <p className="auto-maker-control-error" role="alert">{action.error}</p>}

      {settingsOpen && <section ref={settingsPanelRef} id="auto-maker-settings" className="auto-maker-settings panel" role="dialog" aria-labelledby="auto-maker-title" tabIndex={-1}
        style={settingsPlacement === null ? { visibility: 'hidden' } : settingsPlacement}>
        <div className="panel-head">
          <h2 id="auto-maker-title"><Bot size={17} aria-hidden="true" />自动化做市参数</h2>
          <button type="button" className="icon-btn" aria-label="关闭参数设置" onClick={() => {
            setSettingsOpen(false)
            settingsButtonRef.current?.focus({ preventScroll: true })
          }}>
            <X size={17} aria-hidden="true" />
          </button>
        </div>
        <div className="auto-maker-status">
          <p className="auto-maker-status-label">{auto.status_label}</p>
          {auto.status === 'WARMING_UP' && (
            <div className="auto-maker-warmup">
              <div className="auto-maker-warmup-bar" role="img" aria-label={`预热进度 ${auto.warmup_have}/${auto.warmup_need}`}>
                <span style={{ width: `${warmupPct}%` }} />
              </div>
              <span className="muted">预热 {auto.warmup_have} / {auto.warmup_need} 根 K 线</span>
            </div>
          )}
          {!auto.enabled && engine.position_source === 'STRATEGY' && (
            <p className="notice notice-info">策略持仓仍受止盈止损保护，可在持仓卡片手动平仓。</p>
          )}
          {symbol !== engine.symbol && <p className="auto-maker-elsewhere muted">运行在 {engine.symbol}</p>}
        </div>
        {fields !== null && draft !== null ? <>
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
              <InputField key={field.key} label={field.label}
                unit={field.display_as_percent ? '%' : field.unit ?? ''}
                value={draft[field.key as NumericKey]} inputMode="decimal"
                error={errorFor(field.key)} onBlur={markTouched(field.key)}
                onChange={(value) => setFieldValue(field.key as NumericKey, value)}
                hint={rangeHint(field, auto.max_lookback)} />
            ))}
            <p className="field-hint auto-maker-note">保本 / 移动止损：模拟盘引擎暂不执行，暂不开放。</p>
          </fieldset>
          {dirty && <p className="auto-maker-unsaved">参数尚未保存，顶部开关只切换已保存的配置。</p>}
          {externalNotice && <p className="notice notice-info">参数已在别处更新。</p>}
          <div className="actions auto-maker-actions">
            <button type="button" className="primary" disabled={action.busy || !dirty || built.params === null}
              onClick={() => void applyDraft()}>
              {action.busy ? '保存中…' : '保存参数'}
            </button>
            <button type="button" className="link-btn" disabled={action.busy} onClick={restoreDefaults}>
              <RotateCcw size={14} aria-hidden="true" />恢复默认
            </button>
          </div>
        </> : loadError !== null ? (
          <div className="auto-maker-load-error notice notice-error" role="alert">
            {loadError}
            <button type="button" className="link-btn" onClick={() => setReloadKey((v) => v + 1)}>重试</button>
          </div>
        ) : <p className="panel-empty">正在加载配置…</p>}
        {action.error !== null && <p className="field-error auto-maker-action-error" role="alert">{action.error}</p>}
      </section>}
    </div>
  )
}
