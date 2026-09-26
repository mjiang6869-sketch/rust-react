// 自动化做市（区间做市策略）参数表单的纯函数部分。
//
// # 单位换算，不是价格算术
//
// `equity_pct` 在后端存的是比例（0.1 = 10%），界面按百分数显示、按百分数
// 录入——这只是单位换算（先例见 `manualPlan.ts`），不是止损/止盈那种要
// 靠后端量化的价格计算。真正的参数合法性判断仍由后端
// `RangeMakerParams::validate` 做最终裁决，这里的范围检查只是提前提示，
// 用的边界（`field.min` / `field.max`）也是后端给的，不是前端自己猜的。

import { cmpStr, fromPercent, parse, toPercent } from '../api/decimal.ts'
import type { AutoMakerParams, ParameterInfo } from '../api/types'

/** 可编辑的数值参数键，必须与后端 `EDITABLE_KEYS` 一一对应。 */
export const NUMERIC_KEYS = [
  'lookback',
  'take_profit_bp',
  'stop_buffer_bp',
  'equity_pct',
  'leverage',
  'valid_minutes',
] as const

export type NumericKey = (typeof NUMERIC_KEYS)[number]

/** 整数字段：后端用 `usize`/`i64` 解析，不接受小数点。 */
const INTEGER_KEYS: readonly NumericKey[] = ['lookback', 'valid_minutes']

/** 表单草稿：数值字段是**显示值**（百分比字段是百分数），side_mode 直接是枚举值。 */
export type AutoMakerDraft = { [K in NumericKey]: string } & {
  side_mode: AutoMakerParams['side_mode']
}

function toDisplay(field: ParameterInfo, raw: string): string {
  return field.display_as_percent ? toPercent(parse(raw), 6) : raw
}

/** 由引擎当前生效参数生成草稿（用于初始化，或外部参数变化后重新同步）。 */
export function draftFromParams(params: AutoMakerParams, fields: ParameterInfo[]): AutoMakerDraft {
  const draft = { side_mode: params.side_mode } as AutoMakerDraft
  for (const field of fields) {
    const key = field.key as NumericKey
    draft[key] = toDisplay(field, params[key])
  }
  return draft
}

/**
 * 由字段说明生成默认草稿（「恢复默认」）。
 *
 * `side_mode` 不在可编辑数值字段之列（后端 `EDITABLE_KEYS` 不含它），所以
 * 没有对应的 `field.default`——这里直接用策略自身的默认值
 * `RangeMakerParams::default`（只做多），与后端保持一致。
 */
export function draftFromDefaults(fields: ParameterInfo[]): AutoMakerDraft {
  const draft = { side_mode: 'LONG_ONLY' } as AutoMakerDraft
  for (const field of fields) {
    const key = field.key as NumericKey
    draft[key] = toDisplay(field, field.default)
  }
  return draft
}

function rangeText(field: ParameterInfo): string {
  if (field.display_as_percent) {
    return `${toPercent(parse(field.min), 6)}%–${toPercent(parse(field.max), 6)}%`
  }
  const unit = field.unit ? ` ${field.unit}` : ''
  return `${field.min}–${field.max}${unit}`
}

/**
 * 把草稿转换成可提交的参数，同时做提前校验。
 *
 * 只校验格式（是不是数字/整数）与 `field.min`/`field.max` 给出的范围——
 * 后端 `RangeMakerParams::validate` 仍是最终裁决，这里的错误提示只是让
 * 用户不必等一次网络往返才知道填错了。
 */
export function buildParams(
  draft: AutoMakerDraft,
  fields: ParameterInfo[],
): { params: AutoMakerParams | null; errors: Record<string, string> } {
  const errors: Record<string, string> = {}
  const underlying: Partial<Record<NumericKey, string>> = {}

  for (const field of fields) {
    const key = field.key as NumericKey
    const raw = (draft[key] ?? '').trim()
    const isInteger = INTEGER_KEYS.includes(key)
    const pattern = isInteger ? /^\d+$/ : /^\d+(?:\.\d+)?$/
    if (!pattern.test(raw)) {
      errors[key] = isInteger ? '请输入正整数' : '请输入有效数字'
      continue
    }
    const value = field.display_as_percent ? fromPercent(raw) : raw
    if (cmpStr(value, field.min) < 0 || cmpStr(value, field.max) > 0) {
      errors[key] = `须在 ${rangeText(field)}之间`
      continue
    }
    underlying[key] = value
  }

  // 防御性检查：`fields` 若缺了某个白名单键（后端配置变化），提示刷新而不是
  // 静默丢参数——否则下面按字面量拼 `AutoMakerParams` 会在运行期缺字段。
  for (const key of NUMERIC_KEYS) {
    if (underlying[key] === undefined && errors[key] === undefined) {
      errors[key] = '缺少该参数的说明，请刷新页面重试'
    }
  }

  if (Object.keys(errors).length > 0) return { params: null, errors }

  const params: AutoMakerParams = {
    lookback: underlying.lookback ?? '',
    take_profit_bp: underlying.take_profit_bp ?? '',
    stop_buffer_bp: underlying.stop_buffer_bp ?? '',
    side_mode: draft.side_mode,
    equity_pct: underlying.equity_pct ?? '',
    leverage: underlying.leverage ?? '',
    valid_minutes: underlying.valid_minutes ?? '',
  }
  return { params, errors }
}

/** 草稿是否与当前生效参数不同（用于决定「应用参数」是否可点）。 */
export function isDraftDirty(draft: AutoMakerDraft, params: AutoMakerParams, fields: ParameterInfo[]): boolean {
  const baseline = draftFromParams(params, fields)
  if (draft.side_mode !== baseline.side_mode) return true
  return fields.some((field) => draft[field.key as NumericKey] !== baseline[field.key as NumericKey])
}
