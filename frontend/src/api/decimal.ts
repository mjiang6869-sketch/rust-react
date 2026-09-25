// 定点小数工具。
//
// # 为什么前端需要这个
//
// 后端传来的价格与数量是精确的十进制字符串（Rust 侧用 `rust_decimal`）。
// 前端如果直接 `Number(s)` 再运算，就会退化成 IEEE 754 浮点：
//
// ```js
// Number('0.1') + Number('0.2') === 0.30000000000000004  // true
// ```
//
// 做市的止盈目标是 bp 级（0.0001），这种误差足以让界面显示的价与后端挂出的
// 价不一致。所以显示用的算术也用整数实现。
//
// # 实现方式
//
// 把十进制字符串解析成 `{ digits: bigint, scale: number }`，即
// `digits / 10^scale`。所有运算在大整数上做，最后格式化回字符串。
// `bigint` 是任意精度，所以不会溢出。

export interface Fixed {
  /** 有效数字（含符号）。 */
  digits: bigint
  /** 小数位数。 */
  scale: number
}

/** 解析十进制字符串。接受 `123`、`-1.5`、`.5`、`1e-8`。 */
export function parse(s: string): Fixed {
  const t = s.trim()
  if (t === '') return { digits: 0n, scale: 0 }

  // 处理科学计数法：rust_decimal 在极小值时会输出它
  const eMatch = /^([+-]?(?:\d+\.?\d*|\.\d+))[eE]([+-]?\d+)$/.exec(t)
  if (eMatch) {
    const base = parse(eMatch[1]!)
    const exp = Number(eMatch[2]!)
    return scaleBy(base, exp)
  }

  const neg = t.startsWith('-')
  const body = t.replace(/^[+-]/, '')
  const dot = body.indexOf('.')
  const intPart = dot >= 0 ? body.slice(0, dot) : body
  const fracPart = dot >= 0 ? body.slice(dot + 1) : ''
  const digitsStr = (intPart + fracPart).replace(/^0+(?=\d)/, '') || '0'
  const digits = BigInt(digitsStr) * (neg ? -1n : 1n)
  return { digits, scale: fracPart.length }
}

/** 按 10^exp 缩放。 */
function scaleBy(v: Fixed, exp: number): Fixed {
  if (exp === 0) return v
  if (exp > 0) return { digits: v.digits * 10n ** BigInt(exp), scale: v.scale }
  return { digits: v.digits, scale: v.scale + -exp }
}

/** 对齐两个数的 scale 后返回它们的 digits。 */
function align(a: Fixed, b: Fixed): [bigint, bigint, number] {
  const scale = Math.max(a.scale, b.scale)
  const ad = a.digits * 10n ** BigInt(scale - a.scale)
  const bd = b.digits * 10n ** BigInt(scale - b.scale)
  return [ad, bd, scale]
}

export function add(a: Fixed, b: Fixed): Fixed {
  const [ad, bd, scale] = align(a, b)
  return { digits: ad + bd, scale }
}

export function sub(a: Fixed, b: Fixed): Fixed {
  const [ad, bd, scale] = align(a, b)
  return { digits: ad - bd, scale }
}

export function mul(a: Fixed, b: Fixed): Fixed {
  return { digits: a.digits * b.digits, scale: a.scale + b.scale }
}

/**
 * 除法，保留 `scale` 位小数（向下取整）。
 *
 * 除法必须指定精度——十进制展开可能无限长（1/3）。做市场景下我们只需要
 * 显示精度，所以取足够大的位数（默认 8，覆盖币安的最大价格精度）。
 */
export function div(a: Fixed, b: Fixed, scale = 8): Fixed {
  if (b.digits === 0n) return { digits: 0n, scale }
  // align 已把两者放大到同一 scale，所以只需再放大 10^scale 以保留目标精度。
  const [ad, bd] = align(a, b)
  const shifted = ad * 10n ** BigInt(scale)
  return { digits: shifted / bd, scale }
}

/** 取绝对值。 */
export function abs(a: Fixed): Fixed {
  return { digits: a.digits < 0n ? -a.digits : a.digits, scale: a.scale }
}

/** 比较。返回 -1 / 0 / 1。 */
export function cmp(a: Fixed, b: Fixed): -1 | 0 | 1 {
  const [ad, bd] = align(a, b)
  if (ad < bd) return -1
  if (ad > bd) return 1
  return 0
}

/** 是否为零。 */
export function isZero(a: Fixed): boolean {
  return a.digits === 0n
}

/** 是否为正。 */
export function isPositive(a: Fixed): boolean {
  return a.digits > 0n
}

/** 是否为正。 */
export function isNegative(a: Fixed): boolean {
  return a.digits < 0n
}

/**
 * 格式化为字符串，去掉尾随零。
 *
 * `format(parse('3200.0000'))` → `'3200'`
 * `format(parse('0.00040'))` → `'0.0004'`
 */
export function format(a: Fixed, maxScale?: number): string {
  const scale = maxScale === undefined ? a.scale : Math.min(maxScale, a.scale)
  const neg = a.digits < 0n
  let d = a.digits < 0n ? -a.digits : a.digits

  // 先按目标精度截断（向下取整，丢弃部分不进位）
  let dropped = a.scale - scale
  let kept = d
  if (dropped > 0) {
    kept = d / 10n ** BigInt(dropped)
    dropped = 0
  }

  const s = kept.toString().padStart(scale + 1, '0')
  const intPart = s.slice(0, s.length - scale)
  const fracPart = scale > 0 ? s.slice(s.length - scale).replace(/0+$/, '') : ''
  const body = fracPart === '' ? intPart : `${intPart}.${fracPart}`
  const out = neg && kept !== 0n ? `-${body}` : body
  return out === '-0' ? '0' : out
}

/** 乘以 100，用于把比例显示成百分比。 */
export function toPercent(a: Fixed, dp = 2): string {
  return format(mul(a, parse('100')), dp)
}

// ---------------------------------------------------------------------------
// 便捷函数：直接用字符串
// ---------------------------------------------------------------------------

/** 字符串加法，结果去掉尾随零。 */
export function addStr(a: string, b: string): string {
  return format(add(parse(a), parse(b)))
}

export function subStr(a: string, b: string): string {
  return format(sub(parse(a), parse(b)))
}

export function mulStr(a: string, b: string): string {
  return format(mul(parse(a), parse(b)))
}

/** 字符串比较。 */
export function cmpStr(a: string, b: string): -1 | 0 | 1 {
  return cmp(parse(a), parse(b))
}

/** 是否为正数（字符串形式）。 */
export function isPositiveStr(a: string): boolean {
  return isPositive(parse(a))
}

export function isNegativeStr(a: string): boolean {
  return isNegative(parse(a))
}

/**
 * 带符号显示。正数加 `+`。
 *
 * 盈亏必须能一眼看出方向——只靠颜色不够（色觉障碍、黑白打印都会丢失信息）。
 */
export function signedStr(a: string, dp?: number): string {
  const v = parse(a)
  const s = format(v, dp)
  return isPositive(v) ? `+${s}` : s
}

/** 数值近似（仅用于图表坐标，不用于交易计算）。 */
export function toNumberApprox(a: string): number {
  return Number(a)
}

/** 从数字构造固定小数字符串（仅用于图表坐标与用户输入的往返）。 */
export function fromNumber(n: number, scale = 8): string {
  if (!Number.isFinite(n)) return '0'
  // toFixed 会有舍入误差，但这里只用于图表坐标（像素级精度足够），
  // 真正的交易数值永远走字符串路径。
  return format(parse(n.toFixed(scale)))
}
