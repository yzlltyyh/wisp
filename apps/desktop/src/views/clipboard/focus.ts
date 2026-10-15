/**
 * 剪贴板视图的焦点（「唤出即可打字」、键盘优先）。焦点应当一直在搜索框里（或打开的菜单里），
 * 这里处理两种会把它弄丢的情况（菜单打开 / 关闭时的焦点由组件库的 Menu 负责）：
 *
 * - **挂载时窗口还不可见**：prepare 的同步渲染里窗口隐藏（mock 是 `visibility: hidden`，真实窗口在 panel_ready 里
 *   才 show），`autoFocus` 的 focus() 静默失败；而 `focused` / `active` / `hidden` 在显示时都不变。由
 *   `useRefocusWhenShown` 在面板进入 entering / shown 阶段、窗口重新获得焦点、页面重新可见时补聚焦；
 * - **点了非输入区域**：列表空白处、按钮（恢复监听、确认条的按钮、「在全部类型中搜索」——点完就卸载，焦点落到
 *   body）。`keepSearchFocus` 在 mousedown 时阻止默认行为（焦点不离开搜索框）；可以选中文字的正文（全文预览）
 *   除外，那里点过之后由 `useTypeToSearch` 兜底：焦点不在视图里时，第一个可打印字符 / ⌫ 把焦点送回搜索框并照常
 *   落进去。
 */
import { useEffect, type MouseEvent as ReactMouseEvent, type RefObject } from 'react'

type Ref<T extends HTMLElement> = RefObject<T | null>

/** 焦点是否「丢了」：在 body 上、不在视图里，也不在打开的菜单 / 对话框里 */
export function focusLost(root: HTMLElement): boolean {
  const at = document.activeElement
  if (!at || at === document.body || at === document.documentElement) return true
  if (root.contains(at)) return false
  return at.closest('[role="menu"], [role="dialog"], [role="alertdialog"]') === null
}

/**
 * 面板显示后（entering / shown）、窗口重新获得焦点、页面重新可见时：焦点丢了就补聚焦搜索框。
 * 只在 `enabled`（视图在显示中、面板持有焦点、处于 entering / shown）时监听；prepare 阶段不用 rAF
 */
export function useRefocusWhenShown(
  root: Ref<HTMLElement>,
  search: Ref<HTMLInputElement>,
  enabled: boolean,
): void {
  useEffect(() => {
    if (!enabled) return
    const ensure = () => {
      const el = root.current
      const input = search.current
      if (!el || !input || document.hidden || !focusLost(el)) return
      input.focus({ preventScroll: true })
    }
    ensure()
    // 显示的那一帧：真实窗口的 show 与 key 状态可能比 panel_ready 的返回晚一点
    const frame = requestAnimationFrame(ensure)
    const onVisible = () => {
      if (!document.hidden) ensure()
    }
    window.addEventListener('focus', ensure)
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      cancelAnimationFrame(frame)
      window.removeEventListener('focus', ensure)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [root, search, enabled])
}

/** 文字输入（复选框、单选框除外）与可以选中文字的区域（全文预览的正文）：mousedown 照常 */
const KEEP_DEFAULT =
  'input:not([type="checkbox"]):not([type="radio"]), textarea, [contenteditable], .clip-preview-body'
/** 按钮：点击照常触发，但不拿走焦点（即使在可选中的区域里） */
const CONTROL = 'button, [role="button"], a[href], label'

/**
 * 视图根节点的 mousedown：点在非输入区域（列表空白、行、按钮、横幅、确认条、空状态）时阻止默认行为，焦点留在
 * 搜索框里（键盘继续打字、方向键继续移动）。点击照常触发。
 */
export function keepSearchFocus(e: ReactMouseEvent<HTMLElement>): void {
  if (e.button !== 0) return
  const target = e.target
  if (!(target instanceof Element)) return
  if (!target.closest(CONTROL) && target.closest(KEEP_DEFAULT)) return
  e.preventDefault()
}

/**
 * 焦点不在视图里（点过可选中的正文、按钮卸载之后）时，第一个可打印字符 / ⌫ / ⌦：把焦点送回搜索框，事件照常
 * 落进去（keydown 期间换焦点，字符与删除都作用在新的焦点上）。键盘路由已经处理过的键（defaultPrevented）、
 * 带 ⌘ / Ctrl / Alt 的组合、输入法组合中的按键、模态确认条打开时都不管
 */
export function useTypeToSearch(
  root: Ref<HTMLElement>,
  search: Ref<HTMLInputElement>,
  enabled: boolean,
): void {
  useEffect(() => {
    if (!enabled) return
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.isComposing || e.metaKey || e.ctrlKey || e.altKey) return
      if (e.key.length !== 1 && e.key !== 'Backspace' && e.key !== 'Delete') return
      const el = root.current
      const input = search.current
      if (!el || !input || el.hasAttribute('data-modal') || !focusLost(el)) return
      input.focus({ preventScroll: true })
    }
    // 在键盘路由之后执行（同为 window 捕获阶段，后注册）：路由处理过的键不再落进搜索框
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [root, search, enabled])
}
