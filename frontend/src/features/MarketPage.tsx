// 交易行情页：图表 + 盘口 + 手动下单。
//
// # 布局
// 桌面将图表、盘口、下单并排，持仓与委托合并为可折叠标签区。
// 窄屏依次排列图表、盘口、持仓与下单，保持各面板可读。
//
// # 图表数据与引擎状态是两条独立的流
//
// K 线历史来自公开 REST，实时 K 线、盘口与成交共用行情 WebSocket，
// 持仓与订单来自引擎的 WebSocket。行情与交易状态
// 的刷新频率和失败模式都不同，所以不合并成一个状态——行情断了不该让持仓
// 显示不出来，反之亦然。

import { useMemo, useState } from 'react'
import { CandlestickChart, Radio } from 'lucide-react'

import { cooldownNotice, useMarketFeed } from '../api/marketFeed'
import { INTERVALS, type EngineState } from '../api/types'
import { ChartHost } from '../chart/ChartHost'
import { useKlines } from '../chart/useKlines'
import { num } from '../format'
import { ManualPanel } from './ManualPanel'
import { OrderBook } from './OrderBook'
import { TradingActivity } from './TradingActivity'
import { TradeTape } from './TradeTape'

/** 图表初始周期。 */
const DEFAULT_INTERVAL = '15m'

export interface MarketPageProps {
  engine: EngineState
  hasPosition: boolean
}

export function MarketPage({ engine, hasPosition }: MarketPageProps) {
  const [symbol, setSymbol] = useState(engine.symbol)
  const [draftSymbol, setDraftSymbol] = useState(engine.symbol)
  const [symbolError, setSymbolError] = useState<string | null>(null)
  return (
    <>
      <form className="market-selector" onSubmit={(event) => {
        event.preventDefault()
        const next = draftSymbol.trim().toUpperCase()
        if (!/^[A-Z0-9]{2,30}$/.test(next)) {
          setSymbolError('请输入有效的合约代码，例如 ETHUSDC')
          return
        }
        setSymbolError(null)
        setDraftSymbol(next)
        setSymbol(next)
      }}>
        <label htmlFor="market-symbol">交易对</label>
        <input type="text" id="market-symbol" list="market-symbols" value={draftSymbol}
          onChange={(event) => setDraftSymbol(event.target.value)} autoComplete="off"
          spellCheck={false} aria-describedby={symbolError ? 'symbol-error' : undefined} />
        <datalist id="market-symbols">
          {[...new Set([engine.symbol, 'ETHUSDC', 'BTCUSDC', 'SOLUSDC', 'ETHUSDT', 'BTCUSDT'])].map((value) => <option key={value} value={value} />)}
        </datalist>
        <button type="submit" className="secondary">切换</button>
        <span className={`mode-badge ${engine.mode === 'LIVE' ? 'mode-live' : 'mode-paper'}`}>{engine.mode_label}</span>
        <span className="head-note">UTC+8</span>
        {symbolError && <span id="symbol-error" role="alert">{symbolError}</span>}
      </form>
      <MarketWorkspace key={symbol} engine={engine} hasPosition={hasPosition} symbol={symbol} />
    </>
  )
}

function MarketWorkspace({ engine, hasPosition, symbol }: MarketPageProps & { symbol: string }) {
  const isEngineSymbol = symbol === engine.symbol
  const [interval, setInterval] = useState<string>(DEFAULT_INTERVAL)

  const feed = useMarketFeed(symbol, true, interval)
  const klines = useKlines(symbol, interval, feed.kline, feed.disconnected)

  // 图表上的价位线。转成字符串交给 `OrderLines`——图表层的转换在
  // `ChartHost` 内部发生，这里保持字符串。
  const levels = useMemo(
    () => ({
      entry: isEngineSymbol ? engine.position?.entry_price ?? null : null,
      stop: isEngineSymbol ? engine.position?.stop_price ?? null : null,
      takeProfits: (isEngineSymbol ? engine.position?.rungs ?? [] : [])
        .filter((r) => !r.filled)
        .map((r) => r.price),
    }),
    [engine.position, isEngineSymbol],
  )

  // 后端按时间倒序推送成交；当前价取最新成交原始字符串。
  const lastPrice = feed.trades[0]?.price ?? null

  return (
    <div className="market-page">
      <div className="chart-panel">
        <div className="chart-heading">
          <div>
            <span className="instrument-icon">
              <CandlestickChart size={20} aria-hidden="true" />
            </span>
            <strong>{symbol}</strong>
            <span className="tag">永续</span>
          </div>
          <span className="head-note">
            {isEngineSymbol ? `${engine.instrument.settlement_asset} 本位` : '仅看盘'}
          </span>
        </div>
        <div className="chart-toolbar">
          <div className="interval-group" role="group" aria-label="K 线周期">
            {INTERVALS.map((iv) => (
              <button
                key={iv.value}
                type="button"
                className={
                  interval === iv.value ? 'iv-btn iv-active' : 'iv-btn'
                }
                aria-pressed={interval === iv.value}
                onClick={() => setInterval(iv.value)}
              >
                {iv.label}
              </button>
            ))}
          </div>

          <div className="chart-toolbar-right">
            {lastPrice !== null && (
              <span className="chart-last">
                {num(lastPrice)}
              </span>
            )}
            {feed.book !== null && (
              <span className="muted">
                价差 {num(feed.book.spread_bp, 2)} bp
              </span>
            )}
          </div>
        </div>

        {klines.error !== null && (
          <div className="chart-error" role="alert">
            {klines.error}
          </div>
        )}

        <div className="chart-canvas">
          <ChartHost
            candles={klines.candles}
            lastCandle={klines.lastCandle}
            levels={levels}
            intervalKey={`${symbol}:${interval}`}
            height="100%"
            historyReady={klines.source !== null}
          />
          {klines.candles.length === 0 && klines.lastCandle === null && (
            <div className="chart-placeholder" role="status">
              <CandlestickChart size={28} aria-hidden="true" />
              <strong>
                {klines.loading ? '正在加载行情' : '暂无 K 线数据'}
              </strong>
              <span>行情就绪后将在这里显示</span>
            </div>
          )}
        </div>

        <div className="chart-foot">
          <span>
            <Radio size={12} aria-hidden="true" />{' '}
            {klines.source ?? '等待公开行情来源'} · {klines.candles.length} 根 K
            线
          </span>
          <span>
            {klines.loading
              ? '刷新中…'
              : klines.lastCandle !== null
                ? '末根未收盘'
                : '已收盘'}
          </span>
        </div>
      </div>

      <div className="market-order">
        {isEngineSymbol ? <ManualPanel
          instrument={engine.instrument}
          hasPosition={hasPosition}
          onSubmitted={() => undefined}
          referencePrice={lastPrice}
        /> : <section className="panel watch-only">
          <div className="panel-head">仅看盘</div>
          <p>当前交易引擎运行 {engine.symbol}。{symbol} 可查看行情，暂不支持在此下单。</p>
        </section>}
      </div>
      <div className="market-side">
        {/* 限流等待与真正的失败分开显示：前者是我们**主动**停下的，
            说成"更新失败"会让用户以为程序坏了，进而去反复刷新——
            那正是把限流拖成封禁的行为。 */}
        {feed.cooldownMs > 0 ? (
          <div className="notice notice-warn" role="status">
            {cooldownNotice(feed.cooldownMs)}
          </div>
        ) : (
          feed.error !== null && (
            <div className="notice notice-error" role="alert">
              行情不是实时的，当前数据可能已过期。{feed.error}
            </div>
          )
        )}
        <OrderBook book={feed.book} currentPrice={lastPrice} depth={9} />
        <TradeTape trades={feed.trades} />
      </div>
      <TradingActivity engine={engine} />
    </div>
  )
}
