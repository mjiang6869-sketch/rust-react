// 主界面。
//
// # 布局：左侧导航 + 主区
//
// 左侧固定导航（总览 / 交易行情 / 数据管理 / 回测），主区按 URL 路由切换。
// 这是行情软件的标准布局，理由是**导航项是稳定的、内容是可变的**——
// 顶部标签栏会让内容区在切换时整体位移，而侧边栏只在水平方向占固定宽度。
//
// 行情页使用紧凑工作区，模式显示在交易对选择器旁。
// 账户统计与费率来源可在总览查看，异常与裸露告警仍跨页面显示。

import { useMemo, type MouseEvent } from 'react'
import {
  Activity,
  CandlestickChart,
  Database,
  FlaskConical,
  LayoutDashboard,
} from 'lucide-react'

import type { EngineState, PositionInfo } from './api/types'
import { MarketPage } from './features/MarketPage'
import { OverviewPanel } from './features/OverviewPanel'
import { DataPanel } from './features/DataPanel'
import { BacktestPanel } from './features/BacktestPanel'
import { num, time } from './format'
import { PAGE_PATHS, usePageRoute, type Page } from './routing'
import { useAppState } from './state/store'

const NAV = [
  {
    value: 'overview',
    label: '总览',
    icon: LayoutDashboard,
  },
  {
    value: 'market',
    label: '行情',
    icon: CandlestickChart,
  },
  {
    value: 'data',
    label: '管理',
    icon: Database,
  },
  {
    value: 'backtest',
    label: '回测',
    icon: FlaskConical,
  },
] as const satisfies readonly {
  value: Page
  label: string
  icon: typeof Activity
}[]

export function App() {
  const { engine, error, clearError, refresh } = useAppState()
  const { page, navigate } = usePageRoute()
  const hasPosition = useMemo(
    () =>
      engine !== null &&
      (engine.position !== null || engine.open_orders.length > 0),
    [engine],
  )

  if (engine === null) {
    return (
      <div className="app">
        <div className="loading" role="status">
          <Activity size={32} aria-hidden="true" />
          <h1>{error === null ? '正在连接做市终端' : '暂时无法连接服务'}</h1>
          <p className="muted">
            如果长时间没有响应，请确认 Rust 服务已启动（默认 127.0.0.1:8080）。
          </p>
          {error !== null && <p className="notice notice-error">{error}</p>}
          <button className="secondary" type="button" onClick={refresh}>
            重新连接
          </button>
        </div>
      </div>
    )
  }

  const isLive = engine.mode === 'LIVE'

  return (
    <div className={`app ${isLive ? 'app-live' : ''} ${page === 'market' ? 'app-market' : ''}`}>
      <Sidebar page={page} onNavigate={navigate} />

      <div className="main">
        {/* 告警条：按紧急程度排列，最紧急的在前。 */}
        <Alerts engine={engine} error={error} onClearError={clearError} showTradingNotices={page === 'market'} />

        <main className="content" id="main-content">
          {page === 'overview' && (
            <OverviewPanel engine={engine} onGoToMarket={() => navigate('market')} />
          )}
          {page === 'market' && (
            <MarketPage engine={engine} hasPosition={hasPosition} />
          )}
          {page === 'data' && <DataPanel />}
          {page === 'backtest' && (
            <BacktestPanel
              symbol={engine.symbol}
              initialEquity={engine.initial_equity}
            />
          )}
          {page === null && (
            <div className="route-not-found" role="status">
              <h1>页面不存在</h1>
              <p className="muted">这个地址没有对应的工作区页面。</p>
              <button type="button" className="secondary" onClick={() => navigate('market')}>返回行情</button>
            </div>
          )}
        </main>

        <footer className="footer">
          <span>
            成交模型：{engine.fill_model}（{engine.fill_model_optimism}）
          </span>
          <span>最后行情：{time(engine.last_event_at)}</span>
        </footer>
      </div>
    </div>
  )
}

// ---------------------------------------------------------------------------
// 左侧导航
// ---------------------------------------------------------------------------

function Sidebar({
  page,
  onNavigate,
}: {
  page: Page | null
  onNavigate: (p: Page) => void
}) {
  return (
    <aside className="sidebar">
      <a className="skip-link" href="#main-content">
        跳转到主内容
      </a>
      <div className="sidebar-brand">
        <span className="brand-mark" aria-hidden="true">
          <Activity size={21} />
        </span>
        <div className="brand-copy">
          <span className="brand-text">做市终端</span>
          <span className="brand-caption">RUST CRYPTO</span>
        </div>
      </div>

      <nav id="sidebar-navigation" className="sidebar-nav" aria-label="主导航">
        {NAV.map((item) => {
          const active = page === item.value
          return (
            <a
              key={item.value}
              href={PAGE_PATHS[item.value]}
              className={active ? 'nav-item nav-active' : 'nav-item'}
              title={item.label}
              aria-label={item.label}
              aria-current={active ? 'page' : undefined}
              onClick={(event: MouseEvent<HTMLAnchorElement>) => {
                if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return
                event.preventDefault()
                onNavigate(item.value)
              }}
            >
              <span className="nav-icon" aria-hidden="true">
                <item.icon size={19} strokeWidth={1.8} />
              </span>
              <span className="nav-label">{item.label}</span>
            </a>
          )
        })}
      </nav>
    </aside>
  )
}

// ---------------------------------------------------------------------------
// 告警
// ---------------------------------------------------------------------------

function Alerts({
  engine,
  error,
  onClearError,
  showTradingNotices,
}: {
  engine: EngineState
  error: string | null
  onClearError: () => void
  showTradingNotices: boolean
}) {
  return (
    <>
      {error !== null && (
        <div className="banner banner-error" role="alert">
          <span>{error}</span>
          <button type="button" className="link-btn" onClick={onClearError}>
            关闭
          </button>
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

      {showTradingNotices && engine.stand_down !== null && (
        <div className="banner banner-info">
          策略当前不交易：{engine.stand_down}
        </div>
      )}
    </>
  )
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
