// 展示格式化。
//
// # 两条约定
//
// 1. **数值走定点算术**，不用 `Number()` 做交易相关的计算。见 `api/decimal.ts`。
// 2. **盈亏必须带符号**，不能只靠颜色。色觉障碍或黑白打印时会丢失信息，
//    而盈亏方向是这类界面最重要的信息。
//
// 图表坐标是唯一例外——像素级精度不承载交易语义，用 `Number()` 即可。

import { format as fmt, parse, sub, mul, div, toPercent, cmp } from './api/decimal'

/** 数值展示，去掉尾随零。 */
export function num(v: string | null | undefined, maxDp = 8): string {
  if (v === null || v === undefined || v === '') return '—'
  try {
    return fmt(parse(v), maxDp)
  } catch {
    return v
  }
}

/** 带符号的盈亏。正数加 `+`。 */
export function signed(v: string | null | undefined, maxDp = 4): string {
  if (v === null || v === undefined || v === '') return '—'
  try {
    const f = parse(v)
    const s = fmt(f, maxDp)
    return cmp(f, parse('0')) > 0 ? `+${s}` : s
  } catch {
    return v
  }
}

/** 比例 → 百分比。`0.1` → `10%`。 */
export function pct(v: string | null | undefined, maxDp = 2): string {
  if (v === null || v === undefined || v === '') return '—'
  try {
    return `${toPercent(parse(v), maxDp)}%`
  } catch {
    return v
  }
}

/** 比例 → 基点。`0.0004` → `4bp`。 */
export function bp(v: string | null | undefined, maxDp = 2): string {
  if (v === null || v === undefined || v === '') return '—'
  try {
    const b = mul(parse(v), parse('10000'))
    return `${fmt(b, maxDp)}bp`
  } catch {
    return v
  }
}

/** 手数，去掉多余的零。 */
export function qty(v: string | null | undefined): string {
  return num(v, 8)
}

/** 字节数 → 人类可读。 */
export function bytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`
}

/** 时长（秒）→ 可读。 */
export function duration(secs: number): string {
  if (secs < 60) return `${secs} 秒`
  if (secs < 3600) return `${Math.floor(secs / 60)} 分 ${secs % 60} 秒`
  const h = Math.floor(secs / 3600)
  const m = Math.floor((secs % 3600) / 60)
  return `${h} 小时 ${m} 分`
}

/** 时刻 → UTC+8 可读；不依赖浏览器所在时区。 */
export function time(iso: string | null | undefined): string {
  if (iso === null || iso === undefined || iso === '') return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleString('zh-CN', { hour12: false, timeZone: 'Asia/Shanghai' })
}

/** 只显示时间部分。 */
export function clock(iso: string | null | undefined): string {
  if (iso === null || iso === undefined || iso === '') return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleTimeString('zh-CN', { hour12: false, timeZone: 'Asia/Shanghai' })
}

/**
 * 盈亏的样式类。
 *
 * 返回类名是为了配合符号显示——**不能只靠颜色**，调用处必须同时用
 * `signed()` 输出带符号的文本。
 */
export function pnlClass(v: string | null | undefined): string {
  if (v === null || v === undefined || v === '') return ''
  try {
    const c = cmp(parse(v), parse('0'))
    if (c > 0) return 'pos'
    if (c < 0) return 'neg'
    return ''
  } catch {
    return ''
  }
}

/** 计算两个价的差值与相对差（用于展示滑点、距入场距离等）。 */
export function diff(a: string, b: string): { abs: string; pct: string } | null {
  try {
    const av = parse(a)
    const bv = parse(b)
    const d = sub(av, bv)
    const p = bv.digits === 0n ? parse('0') : div(d, bv, 6)
    return { abs: fmt(d, 8), pct: toPercent(p, 4) }
  } catch {
    return null
  }
}
