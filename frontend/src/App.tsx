// 主界面。
//
// # 布局的优先顺序
//
// 界面按「需要立刻看到」的顺序排列，而不是按功能重要性：
//
// 1. **模式与连接状态** —— 模拟盘还是实盘必须一眼可见，不能靠颜色暗示
// 2. **裸露告警** —— 止损触发未成交时仓位在裸露，这是最紧急的状态
// 3. **持仓与保护单** —— 各档止盈是否已成交、止损挂在哪
// 4. **手动下单** —— 核心操作
// 5. **订单与历史** —— 次要，用标签页收起来
//
// # 模拟盘标识不能省
//
// 交易软件最容易出的严重问题是「以为在模拟盘，其实在实盘」。所以模式用
// 文字 + 颜色双重标识，且实盘时整个顶栏换色。

import { useMemo, useState } from 'react'

import type { PositionInfo } from './api/types'
import { ManualPanel } from './features/ManualPanel'
import { OrdersPanel } from './features/OrdersPanel'
import { PositionCard } from './features/PositionCard'
import { DataPanel } from './features/DataPanel'
import { BacktestPanel } from './features/BacktestPanel'
import { num, pnlClass, signed, time } from './format'
import { useAppState } from './state/store'

type Tab = 'trade' | 'orders' | 'data' | 'backtest'

export function App() {
  const { engine, connected, error, clearError } = useAppState()
  const [tab, setTab] = useState<Tab>('trade')

  const hasPosition = useMemo(
    () => engine !== null && (engine.position !== null || engine.open_orders.length > 0),
    [engine],
  )

  if (engine === null) {
    return (
      <div className="app">
        <div className="loading">
          <p>正在连接后端服务…</p>
          <p className="muted">
            如果长时间没有响应，请确认 Rust 服务已启动（默认 127.0.0.1:8080）。
          </p>
        </div>
      </div>
    )
  }

  const isLive = engine.mode === 'LIVE'
  const pnl = signed(engine.realized_pnl)

  return (
    <div className={`app ${isLive ? 'app-live' : ''}`}>
      {/* 顶栏：模式、连接、权益 */}
      <header className="topbar">
        <div className="brand">
          <span className="symbol">{engine.symbol}</span>
          <span className={`mode-badge ${isLive ? 'mode-live' : 'mode-paper'}`}>
            {engine.mode_label}
          </span>
        </div>

        <div className="topbar-stats">
          <Stat label="权益" value={num(engine.equity, 2)} />
          <Stat
            label="已实现"
            value={pnl}
            className={pnlClass(engine.realized_pnl)}
          />
          <Stat
            label="未实现"
            value={signed(engine.unrealized_pnl)}
            className={pnlClass(engine.unrealized_pnl)}
          />
          <Stat label="手续费" value={num(engine.total_fees, 4)} />
        </div>

        <div className="topbar-status">
          <StatusDot ok={connected} label={connected ? '已连接' : '未连接'} />
          <StatusDot
            ok={engine.feed_fresh}
            label={
              engine.feed_fresh
                ? '行情正常'
                : engine.feed_connected
                  ? '行情陈旧'
                  : '无行情'
            }
          />
        </div>
      </header>

      {/* 错误横幅。后端的错误消息是面向用户的，直接展示。 */}
      {error !== null && (
        <div className="banner banner-error" role="alert">
          <span>{error}</span>
          <button type="button" className="link-btn" onClick={clearError}>
            关闭
          </button>
        </div>
      )}

      {/* 策略让位：说明「为什么不交易」，而不是静默停止。 */}
      {engine.stand_down !== null && (
        <div className="banner banner-info">
          策略当前不交易：{engine.stand_down}
        </div>
      )}

      {/* 费率来源非权威：整个 edge 依赖零费率活动，必须提示。 */}
      {!engine.instrument.fee_is_authoritative && (
        <div className="banner banner-warn">
          费率来源为「{feeSourceLabel(engine.instrument.fee_source)}」，尚未与交易所
          账户对账。当前 maker 费率 {num(engine.instrument.maker_rate, 6)}，
          全部收益都依赖这个假设。
        </div>
      )}

      {/* 裸露告警：最高优先级 */}
      {engine.position?.stop_triggered === true && (
        <div className="banner banner-danger" role="alert">
          <strong>止损已触发但未成交，仓位正在裸露。</strong>
          <span>
            止损是限价挂单，价格跳空穿过它且不回来时不会成交。请检查是否需要
            手动平仓。
          </span>
        </div>
      )}

      <nav className="tabs" role="tablist" aria-label="功能切换">
        <TabButton current={tab} value="trade" onClick={setTab}>
          交易
        </TabButton>
        <TabButton current={tab} value="orders" onClick={setTab}>
          订单与成交
        </TabButton>
        <TabButton current={tab} value="backtest" onClick={setTab}>
          回测
        </TabButton>
        <TabButton current={tab} value="data" onClick={setTab}>
          数据管理
        </TabButton>
      </nav>

      <main className="content">
        {tab === 'trade' && (
          <div className="trade-layout">
            <div className="trade-left">
              <PositionCard position={engine.position} symbol={engine.symbol} />
              <OrdersPanel
                orders={engine.open_orders}
                symbol={engine.symbol}
                compact
                title="在途订单"
              />
            </div>
            <div className="trade-right">
              <ManualPanel
                instrument={engine.instrument}
                hasPosition={hasPosition}
                onSubmitted={() => setTab('orders')}
              />
            </div>
          </div>
        )}

        {tab === 'orders' && (
          <OrdersPanel
            orders={engine.open_orders}
            symbol={engine.symbol}
            title="当前在途订单"
          />
        )}

        {tab === 'backtest' && (
          <BacktestPanel
            symbol={engine.symbol}
            initialEquity={engine.initial_equity}
          />
        )}

        {tab === 'data' && <DataPanel />}
      </main>

      <footer className="footer">
        <span>
          成交模型：{engine.fill_model}（{engine.fill_model_optimism}）
        </span>
        <span>最后行情：{time(engine.last_event_at)}</span>
      </footer>
    </div>
  )
}

function Stat({
  label,
  value,
  className = '',
}: {
  label: string
  value: string
  className?: string
}) {
  return (
    <div className="stat">
      <span className="stat-label">{label}</span>
      <span className={`stat-value ${className}`}>{value}</span>
    </div>
  )
}

function StatusDot({ ok, label }: { ok: boolean; label: string }) {
  return (
    <span className={`status ${ok ? 'status-ok' : 'status-bad'}`}>
      <span className="dot" aria-hidden="true" />
      {label}
    </span>
  )
}

function TabButton({
  current,
  value,
  onClick,
  children,
}: {
  current: Tab
  value: Tab
  onClick: (t: Tab) => void
  children: React.ReactNode
}) {
  const active = current === value
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      className={active ? 'tab tab-active' : 'tab'}
      onClick={() => onClick(value)}
    >
      {children}
    </button>
  )
}

function feeSourceLabel(src: string): string {
  switch (src) {
    case 'EXCHANGE_ACCOUNT':
      return '交易所账户对账'
    case 'EXCHANGE_RULES':
      return '交易所规则'
    case 'PROMOTIONAL_ASSUMED':
      return '零费率活动假设'
    case 'CONFIGURED_DEFAULT':
      return '手工配置'
    default:
      return src
  }
}

/** 供测试与调试：把持仓序列化成可读文本。 */
export function describePosition(p: PositionInfo | null): string {
  if (p === null) return '无持仓'
  const rungs = p.rungs
    .map((r) => `${r.index}:${r.filled ? '已成交' : '挂单中'}@${r.price}`)
    .join(' ')
  return `${p.side_label} ${num(p.quantity)} @ ${num(p.entry_price)} 止损 ${
    p.stop_price === null ? '—' : num(p.stop_price)
  } ${rungs}`
}
