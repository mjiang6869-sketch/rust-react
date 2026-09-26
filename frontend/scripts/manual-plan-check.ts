// node --experimental-strip-types scripts/manual-plan-check.ts
import { manualPlan, type ManualInputs } from '../src/features/manualPlan.ts'

function assert(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message)
}
const base: ManualInputs = {
  symbol: 'ETHUSDC', side: 'BUY', entry: '3200.000000000000000001',
  stopBp: '12.5', stopPrice: '', stopMode: 'bp', leverage: '3', sizePct: '10',
  useLadder: true, rungs: [{ bp: '25', price: '', percent: '40' }, { bp: '50', price: '', percent: '30' }, { bp: '75', price: '', percent: '30' }],
  singleTpBp: '50', singleTpPrice: '', tpMode: 'bp', breakEven: true, cancelSecs: '120',
}
const initial = manualPlan(base)
assert(initial.plan?.entry === base.entry, '价格必须保持原始字符串')
assert(initial.plan.stop === undefined && initial.plan.stop_distance_bp === '12.5', '前端只提交距离意图，不计算止损价')
assert(initial.plan.size_pct === '0.1', '百分比转换')
assert(initial.plan.take_profit?.[0]?.pct === '0.0025' && initial.plan.take_profit[0].fraction === '0.4', '基点和分档比例精确转换')
assert(initial.total === '100', '合计精确')
for (const change of [
  { entry: '' }, { entry: 'NaN' }, { stopBp: '0' }, { stopBp: '10000' },
  { sizePct: '101' }, { leverage: '0.5' }, { cancelSecs: '1.2' },
  { rungs: [{ bp: '', price: '', percent: '50' }] }, { rungs: [{ bp: '25', price: '', percent: '101' }] },
]) assert(manualPlan({ ...base, ...change }).plan === null, `拒绝非法参数 ${JSON.stringify(change)}`)
const small = manualPlan({ ...base, sizePct: '0.000000000001', useLadder: false, cancelSecs: '' }).plan
assert(small?.size_pct === '0.00000000000001', '小比例不被浮点舍入为零')
assert(small.take_profit_pct === '0.005' && small.take_profit === undefined, '单档止盈')
assert(small.cancel_unfilled_after_secs === undefined, '空白取消超时不传')
const fixed = manualPlan({
  ...base, stopMode: 'price', stopPrice: '3188.25', tpMode: 'price',
  rungs: [{ bp: '25', price: '3210', percent: '40' }, { bp: '50', price: '3220', percent: '60' }],
}).plan
assert(fixed?.stop === '3188.25' && fixed.stop_distance_bp === undefined, '指定止损价原样交给后端')
assert(fixed.take_profit_prices?.[0]?.price === '3210' && fixed.take_profit === undefined, '指定止盈价原样交给后端')
console.log('PASS: 意图保留价格精度、单位转换、无前端止损计算、非法输入拦截、单档/分批与自动撤单')
