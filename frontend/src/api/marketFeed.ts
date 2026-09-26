// 实时行情：盘口与成交流。
//
// # 为什么从轮询改成推送（事故复盘）
//
// 之前这里每秒轮询一次 `/market/book` 与 `/market/trades`。后者对应币安
// `/fapi/v1/aggTrades`，权重 20——光它一项每分钟 1200 权重。一个标签页合计
// 约 1380 权重/分钟；切一次标签页多一条定时器链（见 `poller.ts` 文件头），
// 就到了约 2760，越过 2400 的按 IP 上限 → 429 → 继续打 → 418 封禁 20 分钟。
//
// 现在盘口与成交流走后端的推送（`/api/v1/market/stream`）。后端对每个交易对
// 只连一份币安行情推送，不管开了几个标签页；推送**不计 REST 权重**。浏览器
// 这一侧不再有任何针对盘口与成交的定时请求——剩下的 REST 只有 K 线
// （首次历史加载与断线补齐，常态不轮询）。
//
// # 断线时显示什么
//
// 保留最后一帧，同时说明为什么不是实时的（后端帧里的 `notice`，或"与服务
// 端的连接断开"）。清空会让用户以为没有行情；不提示会让用户把陈旧价格当
// 真。两种都比"显示旧数据并说明"糟糕。
//
// # 限流
//
// 后端与币安之间的冷却由后端管理（REST 与推送共用一份）。帧里带着剩余冷却
// 时间，这里把它同步进前端的全局冷却：K 线历史补齐看到冷却会一起停，不必先撞
// 一次 429 才知道。

import { useEffect, useState } from 'react'

import { ApiError } from './client'
import { armCooldown } from './cooldown'
import { browserDeps, createMarketSocket, marketStreamUrl } from './marketSocket'
import type { BookSnapshotResponse, KlineFrame, RecentTrade } from './types'


export interface MarketFeed {
  kline: KlineFrame | null
  disconnected: boolean
  book: BookSnapshotResponse | null
  trades: RecentTrade[]
  /**
   * 为什么当前不是实时的。`null` 表示正常。
   *
   * 行情断了要让用户知道，而不是显示陈旧价格。
   */
  error: string | null
  /** 上次收到行情帧的时刻（本地时钟）。 */
  updatedAt: number | null
  /**
   * 剩余限流冷却毫秒数。`0` 表示没有在等待。
   *
   * 与 `error` 分开是因为语义不同：`error` 是"出错了"，而这是**我们知道
   * 现在不该发请求**——界面上的措辞与颜色都不同，用户也不会误以为程序
   * 崩了。它每秒递减，界面据此显示倒计时。
   */
  cooldownMs: number
  /** 数据来源，界面必须显示。收到第一帧之前为 `null`。 */
  source: string | null
}

/**
 * 订阅盘口与最近成交。
 *
 * `enabled` 为 false 时不连接——页面切到「数据管理」时没必要占着推送。
 */
export function useMarketFeed(symbol: string, enabled: boolean, interval?: string): MarketFeed {
  const [kline, setKline] = useState<KlineFrame | null>(null)
  const [disconnected, setDisconnected] = useState(false)
  const [book, setBook] = useState<BookSnapshotResponse | null>(null)
  const [trades, setTrades] = useState<RecentTrade[]>([])
  const [error, setError] = useState<string | null>(null)
  const [updatedAt, setUpdatedAt] = useState<number | null>(null)
  const [source, setSource] = useState<string | null>(null)
  // 冷却的**截止时刻**。存截止时刻而不是剩余毫秒：冷却期间后端可能很久不
  // 推新帧，若存剩余毫秒，界面上的倒计时会僵住不动。
  const [cooldownUntil, setCooldownUntil] = useState(0)
  const [, setTick] = useState(0)

  useEffect(() => {
    if (!enabled) return

    // 换交易对：旧交易对的盘口不能留在新交易对的标题下面。
    setKline(null)
    setDisconnected(false)
    setBook(null)
    setTrades([])
    setError(null)
    setUpdatedAt(null)

    const socket = createMarketSocket({
      url: marketStreamUrl(symbol, interval),
      symbol,
      deps: browserDeps,
      onFrame(frame): void {
        setKline(frame.kline ?? null)
        setDisconnected(false)
        setBook(frame.book)
        setTrades(frame.trades)
        setSource(frame.source)
        // 刚打开页面时上游还在握手：盘口面板自己会显示"暂无盘口数据"，
        // 这时弹红色告警只会让每次打开页面都像出了故障。
        setError(
          frame.live || frame.connecting ? null : (frame.notice ?? '行情推送尚未就绪'),
        )
        setUpdatedAt(Date.now())
        if (frame.cooldown_ms > 0) {
          armCooldown(frame.cooldown_ms)
          setCooldownUntil(Date.now() + frame.cooldown_ms)
        } else {
          setCooldownUntil(0)
        }
      },
      onDisconnect(retryInMs): void {
        setDisconnected(true)
        setError(
          `与服务端的行情连接断开，约 ${Math.ceil(retryInMs / 1_000)} 秒后重连。` +
            `当前显示的是断开前的数据。`,
        )
      },
    })
    socket.start()
    return () => socket.stop()
  }, [symbol, enabled, interval])

  // 冷却期间每秒重渲染一次，让倒计时动起来。只在真的处于冷却时才装这个
  // 定时器——常态下不该有额外的每秒重渲染。
  useEffect(() => {
    if (cooldownUntil === 0) return
    const timer = setInterval(() => {
      if (Date.now() >= cooldownUntil) {
        setCooldownUntil(0)
        return
      }
      setTick((n) => n + 1)
    }, 1_000)
    return () => clearInterval(timer)
  }, [cooldownUntil])

  const cooldownMs = cooldownUntil > 0 ? Math.max(0, cooldownUntil - Date.now()) : 0

  return { book, trades, error, updatedAt, cooldownMs, source, kline, disconnected }
}

/**
 * 这个失败是不是限流。
 *
 * 两类都算：真正的 HTTP 429（`ApiError.isRateLimited`），以及本地冷却
 * 期间被 `request()` 拦下的 [`CooldownError`]。前者要触发退避，后者说明
 * 已经在退避了。
 */
export function isRateLimited(e: unknown): boolean {
  return e instanceof ApiError && e.isRateLimited
}

/**
 * 冷却提示的最终文案。
 *
 * 冷却期间 K 线那一路也会停（它走同一份冷却），盘口与成交的推送在后端同样
 * 暂停握手，所以这里明确说是**币安的权重限制**；用户看到"停了"却不知道
 * 为什么，会以为程序坏了——这次事故就是这样才被发现的。
 */
export function cooldownNotice(remainingMs: number): string {
  return (
    `币安接口限流，行情刷新已暂停，约 ${Math.ceil(remainingMs / 1_000)} 秒后自动恢复。` +
    `当前显示的是暂停前的数据。`
  )
}
