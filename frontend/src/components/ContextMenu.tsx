import { useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { createPortal } from 'react-dom'

/** 共享右键菜单：视口内定位、键盘导航、点击外部/Escape/滚动关闭。 */
export function ContextMenu({ x, y, label, children, onClose }: {
  x: number; y: number; label: string; children: ReactNode; onClose: () => void
}) {
  const ref = useRef<HTMLDivElement>(null)
  const [position, setPosition] = useState({ left: x, top: y })
  useLayoutEffect(() => {
    const menu = ref.current
    if (!menu) return
    const previous = document.activeElement
    const place = () => {
      const rect = menu.getBoundingClientRect()
      setPosition({ left: Math.max(8, Math.min(x, innerWidth - rect.width - 8)), top: Math.max(8, Math.min(y, innerHeight - rect.height - 8)) })
    }
    place()
    const observer = new ResizeObserver(place)
    observer.observe(menu)
    menu.querySelector<HTMLElement>('[role="menuitem"]')?.focus()
    const outside = (event: PointerEvent) => { if (!menu.contains(event.target as Node)) onClose() }
    const escape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') { event.preventDefault(); onClose(); if (previous instanceof HTMLElement) previous.focus() }
      if (event.key === 'Tab') onClose()
    }
    const onScroll = (event: Event) => { if (!menu.contains(event.target as Node)) onClose() }
    document.addEventListener('pointerdown', outside)
    document.addEventListener('keydown', escape)
    window.addEventListener('resize', onClose)
    window.addEventListener('scroll', onScroll, true)
    return () => {
      observer.disconnect()
      document.removeEventListener('pointerdown', outside)
      document.removeEventListener('keydown', escape)
      window.removeEventListener('resize', onClose)
      window.removeEventListener('scroll', onScroll, true)
    }
  }, [x, y, onClose])
  return createPortal(<div ref={ref} className="context-menu" style={position} role="menu" aria-label={label}
    onContextMenu={(event) => event.preventDefault()} onKeyDown={(event) => {
      if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return
      const items = Array.from(ref.current?.querySelectorAll<HTMLElement>('[role="menuitem"]:not(:disabled)') ?? [])
      if (!items.length) return
      event.preventDefault()
      const current = items.indexOf(document.activeElement as HTMLElement)
      const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1
        : (current + (event.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length
      items[next]?.focus()
    }}>{children}</div>, document.body)
}
