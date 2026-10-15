/**
 * 分组内子视图（服务列表 ⇄ 编辑器、风格列表 ⇄ 编辑器）的滚动位置与焦点：
 * - 进入子视图：滚动容器回到顶部（返回按钮、标题、第一个字段都在视野里），焦点移到子视图标题（`data-subview-title`，
 *   tabIndex=-1，读屏播报新页面；已经有输入框自动获得焦点时不动）；
 * - 返回：恢复离开时列表的滚动位置，焦点回到打开它的那一行（`data-nav-id`），键盘与读屏用户不用从头 Tab。
 * 子视图切换只有新视图的进入动画（旧视图立即卸载），所以 useLayoutEffect 时新视图已经在 DOM 里。
 */
import { useLayoutEffect, useRef, useState } from 'react'

interface Saved {
  scroll: number
  /** 返回时要聚焦的元素（`data-nav-id`） */
  focus: string | null
}

export function useSubviewNav(viewKey: string) {
  const host = useRef<HTMLDivElement>(null)
  const saved = useRef(new Map<string, Saved>())
  const first = useRef(true)
  /** 是否已经离开过初始视图：之后的视图才播进入动画（分组首次显示时由分组切换的淡入负责） */
  const [entered, setEntered] = useState(false)

  /** 离开当前子视图之前调用：记下滚动位置；`returnTo` 是返回时要聚焦的行 */
  const leave = (returnTo: string | null = null) => {
    const scroller = host.current?.closest<HTMLElement>('.st-page-scroll')
    saved.current.set(viewKey, { scroll: scroller?.scrollTop ?? 0, focus: returnTo })
    setEntered(true)
  }

  useLayoutEffect(() => {
    if (first.current) {
      first.current = false
      return
    }
    const root = host.current
    const scroller = root?.closest<HTMLElement>('.st-page-scroll')
    if (!root || !scroller) return
    const back = saved.current.get(viewKey)
    if (back) {
      saved.current.delete(viewKey)
      scroller.scrollTop = back.scroll
      if (back.focus) {
        root
          .querySelector<HTMLElement>(`[data-nav-id="${back.focus.replace(/["\\]/g, '\\$&')}"]`)
          ?.focus({ preventScroll: true })
      }
      return
    }
    scroller.scrollTop = 0
    const active = document.activeElement
    if (active instanceof HTMLInputElement && root.contains(active)) return
    root.querySelector<HTMLElement>('[data-subview-title]')?.focus({ preventScroll: true })
  }, [viewKey])

  return { host, leave, entered }
}
