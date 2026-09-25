// 持仓与订单的价位线。
//
// # 用 price line 而不是自定义 primitive
//
// lightweight-charts 的 `createPriceLine` 已经提供了价格轴上带标签的水平线，
// 并且会随缩放自动定位。自定义 primitive 只有在需要**两点之间的斜线**时才
// 必要（趋势线、矩形等），水平价位线用它属于过度工程。
//
// # 为什么线段颜色之外还要用线型区分
//
// 止损与止盈如果只用红色/绿色区分，色觉障碍用户无法分辨——而这两者的
// 后果完全不同（止损意味着认错离场，止盈意味着获利了结）。所以止损用虚线、
// 止盈用点线，颜色只是辅助。
//
// 这与项目里「不能只显示颜色」的约定一致。

import type { IPriceLine, ISeriesApi, SeriesType } from 'lightweight-charts'
import { LineStyle } from 'lightweight-charts'

import { format as fmtDec, parse as parseDec } from '../api/decimal'

export interface LevelSet {
  entry: string | null
  stop: string | null
  takeProfits: string[]
}

/** 价位线的管理。持有已创建的线以便更新时先移除旧的。 */
export class OrderLines {
  private lines: IPriceLine[] = []

  constructor(private readonly series: ISeriesApi<SeriesType>) {}

  /** 应用一组价位。会清掉之前的线。 */
  apply(levels: LevelSet): void {
    this.clear()

    if (levels.entry !== null) {
      this.add(levels.entry, '入场', '#58a6ff', LineStyle.Solid)
    }
    if (levels.stop !== null) {
      // 虚线：与止盈区分，不依赖颜色
      this.add(levels.stop, '止损', '#f85149', LineStyle.Dashed)
    }
    levels.takeProfits.forEach((price, i) => {
      // 点线：多档之间靠标签序号区分
      this.add(price, `止盈 ${i + 1}`, '#3fb950', LineStyle.Dotted)
    })
  }

  private add(price: string, title: string, color: string, style: LineStyle): void {
    const value = Number(fmtDec(parseDec(price), 8))
    if (!Number.isFinite(value) || value <= 0) return

    const line = this.series.createPriceLine({
      price: value,
      color,
      lineWidth: 1,
      lineStyle: style,
      axisLabelVisible: true,
      title,
    })
    this.lines.push(line)
  }

  clear(): void {
    for (const line of this.lines) {
      try {
        this.series.removePriceLine(line)
      } catch {
        // 图表已销毁时移除会抛错，忽略即可——
        // 这是清理路径，不该因为图表已不存在而失败。
      }
    }
    this.lines = []
  }
}
