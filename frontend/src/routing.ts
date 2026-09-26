import { useCallback, useEffect, useState } from 'react'

export const PAGE_PATHS = {
  overview: '/overview',
  market: '/market',
  data: '/data',
  backtest: '/backtest',
} as const

export type Page = keyof typeof PAGE_PATHS

function pageForPath(pathname: string): Page | null {
  if (pathname === '/') return 'market'
  const normalized = pathname.replace(/\/+$/, '')
  const entry = Object.entries(PAGE_PATHS).find(([, path]) => path === normalized)
  return (entry?.[0] as Page | undefined) ?? null
}

/** 路由只管理界面路径；引擎状态仍由 AppStateProvider 持续维护。 */
export function usePageRoute(): { page: Page | null; navigate: (page: Page) => void } {
  const [pathname, setPathname] = useState(() => window.location.pathname)

  useEffect(() => {
    const onPopState = () => setPathname(window.location.pathname)
    window.addEventListener('popstate', onPopState)
    return () => window.removeEventListener('popstate', onPopState)
  }, [])

  const navigate = useCallback((page: Page) => {
    const path = PAGE_PATHS[page]
    if (window.location.pathname === path) return
    window.history.pushState(null, '', path)
    setPathname(path)
  }, [])

  return { page: pageForPath(pathname), navigate }
}
