// 盘口深度。
//
// # 卖盘倒序，买盘正序
//
// 这是盘口唯一的正确排法：卖盘最便宜的（最优卖价）在**最下面**，买盘最贵的
// （最优买价）在**最上面**。两列在中间价处相接。反过来的盘口是错的——
// 价格从中间向外递增而不是向中间递增，用户会读错方向。
//
// # 深度条宽度用累计量
//
// 单档量看不出厚度：一档 5 个 ETH 可能是"薄"，但如果它前面还有 50 个累积，
// 那就是一堵墙。所以宽度用**累计量**（后端算好的），且两边的基准是同一个
// 数——否则买卖两侧的条长度不可比，而那正是我们要比的。

import type { BookSnapshotResponse, BookLevel } from '../api/types'
import { num } from '../format'

export interface OrderBookProps {
  book: BookSnapshotResponse | null
  /** 每边显示多少档。 */
  currentPrice: string | null
  depth?: number
}

export function OrderBook({ book, currentPrice, depth = 12 }: OrderBookProps) {
  if (book === null) {
    return (
      <div className="panel">
        <div className="panel-head">盘口</div>
        <div className="panel-empty">暂无盘口数据</div>
      </div>
    )
  }

  const asks = book.asks.slice(0, depth)
  const bids = book.bids.slice(0, depth)

  // 两侧共用一个宽度基准，否则条长度不可比。
  const maxCum = Math.max(
    1,
    ...asks.map((l) => Number(l.cumulative)),
    ...bids.map((l) => Number(l.cumulative)),
  )


  return (
    <div className="panel book">
      <div className="panel-head">
        盘口
        <span className="head-note">价差 {num(book.spread_bp, 2)} bp</span>
      </div>

      <div className="book-cols">
        <span>价格</span>
        <span>数量</span>
        <span>累计</span>
      </div>

      <div className="book-rows">
        {/* 卖盘：倒序，最优卖价紧贴中间价 */}
        {[...asks].reverse().map((l) => (
          <BookRow key={`a${l.price}`} level={l} side="ask" maxCum={maxCum} />
        ))}
      </div>

      <div className="book-mid">
        <span className="book-mid-price">
          {num(currentPrice)}
        </span>
        <span className="muted">当前价</span>
      </div>

      <div className="book-rows">
        {bids.map((l) => (
          <BookRow key={`b${l.price}`} level={l} side="bid" maxCum={maxCum} />
        ))}
      </div>
    </div>
  )
}

function BookRow({
  level,
  side,
  maxCum,
}: {
  level: BookLevel
  side: 'bid' | 'ask'
  maxCum: number
}) {
  const pct = (Number(level.cumulative) / maxCum) * 100
  return (
    <div className={`book-row book-${side}`}>
      {/* 深度条作为背景层，宽度按累计量 */}
      <span className="book-bar" style={{ width: `${pct}%` }} aria-hidden="true" />
      <span className="book-price">{num(level.price, 2)}</span>
      <span className="book-qty">{num(level.quantity, 3)}</span>
      <span className="book-cum">{num(level.cumulative, 2)}</span>
    </div>
  )
}
