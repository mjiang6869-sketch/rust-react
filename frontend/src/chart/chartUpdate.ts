import type { CandleBar } from '../api/types'

/** 只有末根更新或向右追加才可用 series.update；向左补历史必须 setData。 */
export function chartUpdatePlan(previous: CandleBar[], next: CandleBar[], sameInterval: boolean) {
  const from = next.findIndex((bar, index) => !sameBar(bar, previous[index]))
  if (sameInterval && from === -1 && next.length === previous.length) {
    return { kind: 'skip' as const, from }
  }
  const last = previous.at(-1)
  const firstUpdate = next[from]
  if (sameInterval && last !== undefined && firstUpdate !== undefined &&
      next.length >= previous.length && from >= previous.length - 1 && firstUpdate.time >= last.time) {
    return { kind: 'update' as const, from }
  }
  return { kind: 'replace' as const, from: 0 }
}

function sameBar(a: CandleBar, b: CandleBar | undefined): boolean {
  return b !== undefined && a.time === b.time && a.open === b.open && a.high === b.high &&
    a.low === b.low && a.close === b.close && a.volume === b.volume && a.closed === b.closed
}
