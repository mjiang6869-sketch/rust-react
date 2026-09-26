// 主界面。
//
// # 布局：左侧导航 + 主区
//
// 左侧固定导航（总览 / 交易行情 / 数据管理 / 回测），主区按选中项切换。
// 这是行情软件的标准布局，理由是**导航项是稳定的、内容是可变的**——
// 顶部标签栏会让内容区在切换时整体位移，而侧边栏只在水平方向占固定宽度。
//
// 行情页使用紧凑工作区，模式显示在交易对选择器旁。
// 账户统计与费率来源可在总览查看，异常与裸露告警仍跨页面显示。

import { useMemo, useState } from 'react'
import {
  Activity,
  ArrowUpRight,
  CandlestickChart,
  Database,
  FlaskConical,
  LayoutDashboard,
  ShieldCheck,
} from 'lucide-react'

import type { EngineState, PositionInfo } from './api/types'
import { MarketPage } from './features/MarketPage'
import { OrdersPanel } from './features/OrdersPanel'
import { PositionCard } from './features/PositionCard'
import { DataPanel } from './features/DataPanel'
import { BacktestPanel } from './features/BacktestPanel'
import { num, pnlClass, signed, time } from './format'
import { useAppState } from './state/store'

type Page = 'overview' | 'market' | 'data' | 'backtest'

const NAV = [
  {
    value: 'overview',
    label: '总览',
    icon: LayoutDashboard,
    description: '账户、持仓与策略运行状态，一目了然。',
  },
  {
    value: 'market',
    label: '行情',
    icon: CandlestickChart,
    description: '观察市场，规划每一笔交易。',
  },
  {
    value: 'data',
    label: '管理',
    icon: Database,
    description: '管理本地历史归档，为可复现的研究做好准备。',
  },
  {
    value: 'backtest',
    label: '回测',
    icon: FlaskConical,
    description: '对比成交假设，检验策略的真实表现。',
  },
] as const satisfies readonly {
  value: Page
  label: string
  icon: typeof Activity
  description: string
}[]

export function App() {
  const { engine, connected, error, clearError, refresh } = useAppState()
  const [page, setPage] = useState<Page>('market')
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
      <Sidebar page={page} onNavigate={setPage} />

      <div className="main">
        {page !== 'market' && <TopBar engine={engine} connected={connected} />}

        {/* 告警条：按紧急程度排列，最紧急的在前。 */}
        <Alerts engine={engine} error={error} onClearError={clearError} showFeeNotice={page !== 'market'} />

        <main className="content" id="main-content">
          {page !== 'market' && <div className="page-heading">
            <div>
              <div className="eyebrow">WORKSPACE / {page.toUpperCase()}</div>
              <h1>{NAV.find((item) => item.value === page)?.label}</h1>
              <p>{NAV.find((item) => item.value === page)?.description}</p>
            </div>
            <span className="workspace-chip">
              <ShieldCheck size={15} aria-hidden="true" />
              {engine.mode_label}工作区
            </span>
          </div>
          }
          {page === 'overview' && (
            <Overview engine={engine} onGoToMarket={() => setPage('market')} />
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
  page: Page
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
            <button
              key={item.value}
              type="button"
              className={active ? 'nav-item nav-active' : 'nav-item'}
              title={item.label}
              aria-label={item.label}
              aria-current={active ? 'page' : undefined}
              onClick={() => onNavigate(item.value)}
            >
              <span className="nav-icon" aria-hidden="true">
                <item.icon size={19} strokeWidth={1.8} />
              </span>
              <span className="nav-label">{item.label}</span>
            </button>
          )
        })}
      </nav>
    </aside>
  )
}

// ---------------------------------------------------------------------------
// 顶栏
// ---------------------------------------------------------------------------

function TopBar({
  engine,
  connected,
}: {
  engine: EngineState
  connected: boolean
}) {
  const isLive = engine.mode === 'LIVE'
  const pnl = signed(engine.realized_pnl)

  return (
    <header className="topbar">
      <div className="topbar-symbol">
        <span className="symbol">{engine.symbol}</span>
        {/* 模式是文字 + 颜色双重标识，不只靠颜色 */}
        <span className={`mode-badge ${isLive ? 'mode-live' : 'mode-paper'}`}>
          {engine.mode_label}
        </span>
        <span className="topbar-contract">
          {engine.instrument.settlement_asset} 本位
        </span>
      </div>

      <div className="topbar-stats">
        <Stat
          label={`账户权益 · ${engine.instrument.settlement_asset}`}
          value={num(engine.equity, 2)}
        />
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
        <Stat label="maker 费率" value={num(engine.instrument.maker_rate, 6)} />
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
  )
}

function Alerts({
  engine,
  error,
  onClearError,
  showFeeNotice,
}: {
  engine: EngineState
  error: string | null
  onClearError: () => void
  showFeeNotice: boolean
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

      {engine.stand_down !== null && (
        <div className="banner banner-info">
          策略当前不交易：{engine.stand_down}
        </div>
      )}

      {/* 费率来源非权威：整个 edge 依赖零费率活动，必须提示。 */}
      {showFeeNotice && !engine.instrument.fee_is_authoritative && (
        <div className="banner banner-warn">
          费率来源为「{feeSourceLabel(engine.instrument.fee_source)}
          」，尚未与交易所 账户对账。当前 maker 费率{' '}
          {num(engine.instrument.maker_rate, 6)}， 全部收益都依赖这个假设。
        </div>
      )}
    </>
  )
}

// ---------------------------------------------------------------------------
// 总览
// ---------------------------------------------------------------------------

function Overview({
  engine,
  onGoToMarket,
}: {
  engine: EngineState
  onGoToMarket: () => void
}) {
  return (
    <div className="overview">
      <PositionCard position={engine.position} symbol={engine.symbol} />

      <div className="overview-grid">
        <div className="panel">
          <div className="panel-head">当前状态</div>
          <dl className="kv">
            <Row k="模式" v={`${engine.mode_label}（${engine.mode}）`} />
            <Row k="交易对" v={engine.symbol} />
            <Row k="权益" v={num(engine.equity, 2)} />
            <Row k="初始权益" v={num(engine.initial_equity, 2)} />
            <Row
              k="已实现盈亏"
              v={signed(engine.realized_pnl)}
              cls={pnlClass(engine.realized_pnl)}
            />
            <Row
              k="未实现盈亏"
              v={signed(engine.unrealized_pnl)}
              cls={pnlClass(engine.unrealized_pnl)}
            />
            <Row k="累计手续费" v={num(engine.total_fees, 4)} />
            <Row k="在途订单" v={`${engine.open_orders.length} 张`} />
          </dl>
        </div>

        <div className="panel">
          <div className="panel-head">成交模型与费率</div>
          <dl className="kv">
            <Row k="成交模型" v={engine.fill_model} />
            <Row k="乐观程度" v={engine.fill_model_optimism} />
            <Row k="maker 费率" v={num(engine.instrument.maker_rate, 6)} />
            <Row k="taker 费率" v={num(engine.instrument.taker_rate, 6)} />
            <Row
              k="费率来源"
              v={feeSourceLabel(engine.instrument.fee_source)}
            />
            <Row
              k="是否权威"
              v={engine.instrument.fee_is_authoritative ? '是' : '否（未对账）'}
              cls={engine.instrument.fee_is_authoritative ? '' : 'warn'}
            />
            <Row
              k="维持保证金率"
              v={`${num(engine.instrument.maint_margin_pct, 2)}%`}
            />
          </dl>
        </div>

        <div className="panel">
          <div className="panel-head">
            在途订单
            <button type="button" className="link-btn" onClick={onGoToMarket}>
              去看盘 <ArrowUpRight size={14} aria-hidden="true" />
            </button>
          </div>
          {engine.open_orders.length === 0 ? (
            <div className="panel-empty">没有在途订单</div>
          ) : (
            <OrdersPanel
              orders={engine.open_orders}
              symbol={engine.symbol}
              compact
              title=""
            />
          )}
        </div>
      </div>
    </div>
  )
}

function Row({ k, v, cls = '' }: { k: string; v: string; cls?: string }) {
  return (
    <div className="kv-row">
      <dt>{k}</dt>
      <dd className={cls}>{v}</dd>
    </div>
  )
}

// ---------------------------------------------------------------------------
// 小组件
// ---------------------------------------------------------------------------

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

function feeSourceLabel(src: string): string {
  switch (src) {
    case 'EXCHANGE_ACCOUNT':
    case 'ExchangeAccount':
      return '交易所账户对账'
    case 'EXCHANGE_RULES':
    case 'ExchangeRules':
      return '交易所规则'
    case 'PROMOTIONAL_ASSUMED':
    case 'PromotionalAssumed':
      return '零费率活动假设'
    case 'CONFIGURED_DEFAULT':
    case 'ConfiguredDefault':
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
