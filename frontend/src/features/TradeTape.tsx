// 最近成交。
//
// # 主动方向是唯一重要的信息
//
// `is_buyer_maker` 为 true 表示**卖方主动**吃掉了买单（价格倾向下行），
// false 表示买方主动。这不是装饰——做市挂单就是等着被主动单吃，成交流的
// 主动方向直接告诉我们"现在谁在推价格"。
//
// 颜色沿用国内习惯：红色涨/买、绿色跌/卖。这与国际平台的绿涨红跌相反，
// 但界面上同时有箭头的方向，不依赖颜色单一线索。

import type { RecentTrade } from '../api/types'
import { num } from '../format'

export interface TradeTapeProps {
  trades: RecentTrade[]
}

export function TradeTape({ trades }: TradeTapeProps) {
  return (
    <div className="panel tape">
      <div className="panel-head">最近成交<span className="head-note">UTC+8</span></div>

      {trades.length === 0 ? (
        <div className="panel-empty">暂无成交</div>
      ) : (
        <>
          <div className="tape-cols">
            <span>价格</span>
            <span>数量</span>
            <span>时间</span>
          </div>
          <div className="tape-rows">
            {trades.map((t) => {
              // 卖方主动 -> 价格下行 -> 绿色
              const aggressiveSide = t.is_buyer_maker ? 'SELL' : 'BUY'
              return (
                <div key={t.trade_id} className={`tape-row tape-${aggressiveSide}`}>
                  <span className="tape-price">
                    {/* 箭头是给色觉障碍用户的第二线索 */}
                    <span className="tape-arrow" aria-hidden="true">
                      {aggressiveSide === 'BUY' ? '↑' : '↓'}
                    </span>
                    {num(t.price, 2)}
                  </span>
                  <span className="tape-qty">{num(t.quantity, 3)}</span>
                  <span className="tape-time">{clock(t.time)}</span>
                </div>
              )
            })}
          </div>
        </>
      )}
    </div>
  )
}

/**
 * 毫秒时间戳字符串 → `HH:MM:SS`。
 *
 * 只取时分秒不取日期：成交流全是最近几分钟的，"今天/昨天"没有信息量。
 */
function clock(ms: string): string {
  const n = Number(ms)
  if (!Number.isFinite(n)) return '—'
  if (Number.isNaN(new Date(n).getTime())) return '—'
  return new Date(n).toLocaleTimeString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false })
}
