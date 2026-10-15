/**
 * 传统滚动条的宽度。正文的滚动容器（StreamView，`.ui-scroll` 的 `scrollbar-gutter: stable`）在 Windows /
 * Linux 的细滚动条、macOS「始终显示滚动条」时会常驻一条槽，正文列因此比头部、底栏窄，右缘对不齐 ✕ / 底栏图标、
 * CJK 提前一个字换行。translate.css 把滚动容器向右伸进卡片的右内边距，再按这里测得的宽度补回内边距：正文列宽恒等于
 * 视图宽度，滚动条落在卡片右边缘（macOS 覆盖式滚动条测得 0）。
 *
 * 测量不放在面板 prepare 的同步路径里：模块加载后空闲时测一次，之后每次视图显示后空闲时复核（系统设置可能变了）。
 */
import { useSyncExternalStore } from 'react'

let width = 0
const listeners = new Set<() => void>()

/** 立即测量（一次强制排版：只在空闲时调用） */
export function measureScrollbar(): void {
  if (typeof document === 'undefined' || !document.body) return
  const probe = document.createElement('div')
  probe.setAttribute('aria-hidden', 'true')
  probe.style.cssText =
    'position:absolute;top:0;left:0;width:100px;height:40px;overflow-y:scroll;scrollbar-gutter:stable;visibility:hidden;pointer-events:none;contain:strict'
  document.body.append(probe)
  const next = Math.max(0, probe.offsetWidth - probe.clientWidth)
  probe.remove()
  if (next === width) return
  width = next
  for (const l of listeners) l()
}

/** 空闲时测量；返回取消函数 */
export function measureScrollbarWhenIdle(): () => void {
  if (typeof window === 'undefined') return () => undefined
  if (typeof requestIdleCallback === 'function') {
    const id = requestIdleCallback(measureScrollbar)
    return () => cancelIdleCallback(id)
  }
  const id = setTimeout(measureScrollbar, 0)
  return () => clearTimeout(id)
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

const get = () => width

/** 当前测得的滚动条宽度（px） */
export function useScrollbarWidth(): number {
  return useSyncExternalStore(subscribe, get, get)
}

// 面板窗口启动时（隐藏预热中）就测好，第一次唤出时已经是正确的值
measureScrollbarWhenIdle()
