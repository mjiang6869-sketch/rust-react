// 应用状态。
//
// # 为什么不用状态管理库
//
// 这个应用的状态形状很简单：一份引擎状态（来自 WebSocket）+ 几个界面局部
// 状态（选中标签、表单值）。引入 zustand/redux 只会增加一层间接，而真正的
// 复杂度在图表（那个刻意放在 React 之外）。
//
// # K 线数据不放在这里
//
// 行情数据直接进图表 series，不经过 React state。每 500ms 推送一次、每次
// 几百根 K 线，走 React 会让整棵树重渲染——那是图表应用最常见的性能陷阱。
// 见 `chart/ChartHost.tsx`。

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import type { ReactNode } from 'react'

import { ApiError, api } from '../api/client'
import { EngineSocket } from '../api/ws'
import type { EngineState, ProgressMessage } from '../api/types'

export interface AppState {
  /** 引擎状态。`null` 表示尚未收到首次快照。 */
  engine: EngineState | null
  /** WebSocket 是否连接。 */
  connected: boolean
  /** 最近一次错误（面向用户的中文说明）。 */
  error: string | null
  /** 后台任务进度。 */
  progress: ProgressMessage | null
  /** 手动清除错误。 */
  clearError: () => void
  /** 重新加载引擎状态（WebSocket 尚未连上时用）。 */
  refresh: () => void
}

const Ctx = createContext<AppState | null>(null)

export function AppStateProvider({ children }: { children: ReactNode }) {
  const [engine, setEngine] = useState<EngineState | null>(null)
  const [connected, setConnected] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [progress, setProgress] = useState<ProgressMessage | null>(null)
  const socketRef = useRef<EngineSocket | null>(null)

  // WebSocket 状态更新。
  //
  // 后端推的是完整状态（快照与增量都是），所以直接替换而非合并。
  // 用函数式更新以避免闭包捕获陈旧值。
  const handleState = useCallback((incoming: Partial<EngineState>) => {
    setEngine((prev) => {
      // 增量里可能只有部分字段，与已有状态合并——
      // 这样即使后端将来改成差异化推送也不会丢字段。
      if (prev === null) return incoming as EngineState
      return { ...prev, ...incoming }
    })
  }, [])

  useEffect(() => {
    const socket = new EngineSocket({
      onState: handleState,
      onProgress: (p) => setProgress(p),
      onConnectionChange: (c) => setConnected(c),
      onError: (_code, message) => setError(message),
    })
    socketRef.current = socket
    socket.connect()

    return () => {
      socket.close()
      socketRef.current = null
    }
  }, [handleState])

  // 首次加载用 REST 拉一次，让界面立刻有数据而不必等 WebSocket 握手。
  useEffect(() => {
    let cancelled = false
    api
      .state()
      .then((s) => {
        if (!cancelled) setEngine(s)
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setError(
          e instanceof ApiError
            ? e.message
            : `加载引擎状态失败：${e instanceof Error ? e.message : String(e)}`,
        )
      })
    return () => {
      cancelled = true
    }
  }, [])

  const refresh = useCallback(() => {
    api
      .state()
      .then(setEngine)
      .catch((e: unknown) => {
        setError(e instanceof ApiError ? e.message : String(e))
      })
  }, [])

  const clearError = useCallback(() => setError(null), [])

  const value = useMemo<AppState>(
    () => ({ engine, connected, error, progress, clearError, refresh }),
    [engine, connected, error, progress, clearError, refresh],
  )

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>
}

export function useAppState(): AppState {
  const v = useContext(Ctx)
  if (v === null) {
    throw new Error('useAppState 必须在 AppStateProvider 内使用')
  }
  return v
}

/**
 * 异步操作的状态管理。
 *
 * 把「忙碌 → 成功/失败 → 反馈」这套模式收敛到一处。每个按钮都手写一遍
 * try/catch/busy 是重复且有遗漏风险的做法（遗漏就会让按钮永远卡在禁用态）。
 */
export interface ActionState {
  busy: boolean
  error: string | null
  /** 执行一个异步操作。失败时把可读原因写入 `error`。 */
  run: <T>(fn: () => Promise<T>) => Promise<T | undefined>
  clear: () => void
}

export function useAction(): ActionState {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const mounted = useRef(true)

  useEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
    }
  }, [])

  const run = useCallback(async <T,>(fn: () => Promise<T>): Promise<T | undefined> => {
    setBusy(true)
    setError(null)
    try {
      const result = await fn()
      return result
    } catch (e: unknown) {
      if (mounted.current) {
        // 后端返回的错误消息是面向用户的，直接用；不覆盖成通用文案。
        setError(e instanceof ApiError ? e.message : `操作失败：${String(e)}`)
      }
      return undefined
    } finally {
      if (mounted.current) setBusy(false)
    }
  }, [])

  const clear = useCallback(() => setError(null), [])

  return { busy, error, run, clear }
}
