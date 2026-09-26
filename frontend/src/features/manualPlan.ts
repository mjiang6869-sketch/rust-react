import { add, cmpStr, div, format, parse } from '../api/decimal.ts'
import type { ManualPlanRequest, Side } from '../api/types'

export interface RungInput { bp: string; price: string; percent: string }
export interface ManualInputs {
  symbol: string
  side: Side
  entry: string
  stopBp: string
  stopPrice: string
  stopMode: 'bp' | 'price'
  leverage: string
  sizePct: string
  rungs: RungInput[]
  useLadder: boolean
  singleTpBp: string
  singleTpPrice: string
  tpMode: 'bp' | 'price'
  breakEven: boolean
  cancelSecs: string
}

/** 只转换比例单位；所有价格计算与量化都在后端完成。 */
export function manualPlan(input: ManualInputs): { plan: ManualPlanRequest | null; errors: Record<string, string>; total: string } {
  const errors: Record<string, string> = {}
  const positive = (key: string, value: string, label: string) => {
    if (!/^\d+(?:\.\d+)?$/.test(value.trim()) || cmpStr(value, '0') <= 0) {
      errors[key] = `请输入大于 0 的${label}`
      return false
    }
    return true
  }
  positive('entry', input.entry, '价格')
  if (input.stopMode === 'bp') {
    if (positive('stopBp', input.stopBp, '止损距离') && cmpStr(input.stopBp, '10000') >= 0) errors.stopBp = '止损距离须小于 10000 基点'
  } else positive('stopPrice', input.stopPrice, '止损价')
  if (positive('leverage', input.leverage, '杠杆') && cmpStr(input.leverage, '1') < 0) errors.leverage = '杠杆不能小于 1 倍'
  if (positive('sizePct', input.sizePct, '仓位比例') && cmpStr(input.sizePct, '100') > 0) errors.sizePct = '仓位比例不能超过 100%'
  let total = parse('0')
  if (input.useLadder) {
    if (input.rungs.length === 0) errors.rungs = '至少保留一档止盈'
    input.rungs.forEach((r, i) => {
      positive(`${input.tpMode}-${i}`, input.tpMode === 'bp' ? r.bp : r.price, input.tpMode === 'bp' ? '止盈距离' : '止盈目标价')
      if (positive(`percent-${i}`, r.percent, '平仓比例')) total = add(total, parse(r.percent))
    })
    if (cmpStr(format(total), '100') > 0) errors.rungs = '合计平仓比例不能超过 100%'
  } else if (input.tpMode === 'bp') positive('singleTpBp', input.singleTpBp, '止盈距离')
  else positive('singleTpPrice', input.singleTpPrice, '止盈目标价')
  if (input.cancelSecs.trim() && (!/^\d+$/.test(input.cancelSecs) || !Number.isSafeInteger(Number(input.cancelSecs)) || Number(input.cancelSecs) <= 0)) errors.cancelSecs = '请输入正整数秒数，或留空不自动撤销'
  if (Object.keys(errors).length) return { plan: null, errors, total: format(total) }
  const ratio = (value: string, base: string) => format(div(parse(value), parse(base), parse(value).scale + base.length))
  const plan: ManualPlanRequest = {
    symbol: input.symbol, side: input.side, entry: input.entry.trim(),
    leverage: input.leverage.trim(),
    size_pct: ratio(input.sizePct, '100'), client_ref: 'manual',
  }
  if (input.stopMode === 'bp') plan.stop_distance_bp = input.stopBp.trim()
  else plan.stop = input.stopPrice.trim()
  if (input.useLadder) {
    if (input.tpMode === 'bp') plan.take_profit = input.rungs.map((r) => ({ pct: ratio(r.bp, '10000'), fraction: ratio(r.percent, '100') }))
    else plan.take_profit_prices = input.rungs.map((r) => ({ price: r.price.trim(), fraction: ratio(r.percent, '100') }))
  } else if (input.tpMode === 'bp') plan.take_profit_pct = ratio(input.singleTpBp, '10000')
  else plan.take_profit_price = input.singleTpPrice.trim()
  if (input.breakEven) plan.break_even = { trigger_r: '1', offset: '0' }
  if (input.cancelSecs.trim()) plan.cancel_unfilled_after_secs = Number(input.cancelSecs)
  return { plan, errors, total: format(total) }
}
