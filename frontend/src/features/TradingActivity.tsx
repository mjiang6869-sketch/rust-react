import { useState } from 'react'
import { ChevronDown, ChevronUp } from 'lucide-react'

import type { EngineState } from '../api/types'
import { OrdersPanel } from './OrdersPanel'
import { PositionCard } from './PositionCard'

/** 引擎实际持仓与委托独立于看盘交易对，折叠只改变显示。 */
export function TradingActivity({ engine }: { engine: EngineState }) {
  const [tab, setTab] = useState('positions')
  const [expanded, setExpanded] = useState(true)
  const tabs = [
    { id: 'positions', label: '当前持仓', count: engine.position === null ? 0 : 1 },
    { id: 'orders', label: '当前委托', count: engine.open_orders.length },
  ]

  return (
    <section className="market-lower activity-panel" aria-label="持仓与委托">
      <div className="activity-heading">
        <div role="tablist" aria-label="交易记录" className="activity-tabs">
          {tabs.map((item, index) => (
            <button type="button" key={item.id} role="tab" id={`tab-${item.id}`}
              aria-controls={`activity-${item.id}`} aria-selected={tab === item.id}
              tabIndex={tab === item.id ? 0 : -1}
              onClick={() => { setTab(item.id); setExpanded(true) }}
              onKeyDown={(event) => {
                let next = index
                if (event.key === 'ArrowRight') next = (index + 1) % tabs.length
                else if (event.key === 'ArrowLeft') next = (index + tabs.length - 1) % tabs.length
                else if (event.key === 'Home') next = 0
                else if (event.key === 'End') next = tabs.length - 1
                else return
                event.preventDefault()
                const id = tabs[next]?.id
                if (id) {
                  setTab(id)
                  setExpanded(true)
                  document.getElementById(`tab-${id}`)?.focus()
                }
              }}>
              {item.label}<span className="activity-count">{item.count}</span>
            </button>
          ))}
        </div>
        <span className="head-note activity-symbol">{engine.symbol}</span>
        <button type="button" className="activity-toggle" aria-expanded={expanded}
          aria-controls="activity-body" onClick={() => setExpanded((value) => !value)}>
          {expanded ? <ChevronDown size={16} /> : <ChevronUp size={16} />}
          {expanded ? '折叠' : '展开'}
        </button>
      </div>
      <div id="activity-body" hidden={!expanded}>
        <div role="tabpanel" id="activity-positions" aria-labelledby="tab-positions" hidden={tab !== 'positions'} tabIndex={0}>
          <PositionCard position={engine.position} symbol={engine.symbol} compact />
        </div>
        <div role="tabpanel" id="activity-orders" aria-labelledby="tab-orders" hidden={tab !== 'orders'} tabIndex={0}>
          <OrdersPanel orders={engine.open_orders} symbol={engine.symbol} compact title="当前委托" />
        </div>
      </div>
    </section>
  )
}
