/**
 * 菜单打开期间的额外按键（风格菜单 / ⋯ 菜单 / 语言菜单共用）。
 *
 * 菜单是键盘路由里的 modal 作用域，视图层的绑定（⌘1–9 直选风格等）此时够不到。不注册成键盘路由的作用域：同层作用域按
 * 注册先后排序，而菜单的作用域在第一次打开时（StrictMode 的 effect 双调用、或菜单自己晚一步启用）可能排在后面、把按键
 * 吞掉。这里在 window 捕获阶段、键盘路由**之后**接收菜单没有处理（没有 preventDefault）的按键，与注册顺序无关。
 *
 * **浏览器加速键**：Windows 的 WebView2 默认启用浏览器加速键（⌘R / Ctrl+R 重载、Ctrl+Shift+R 强制重载、Ctrl+P 打印
 * 预览）：菜单打开时一律吞掉，不执行任何动作。IME 组合输入期间放行。
 */
import { useEffect, useLayoutEffect, useRef } from 'react'
import { boot } from '../ipc'
import { matchesEvent, parseCombo } from './combo'
import type { KeyBinding } from './keyRouter'

/** 吞掉按键、不执行任何动作（返回 undefined：键盘路由照常 preventDefault） */
export const swallow = (): undefined => undefined

/** 会触发 WebView 默认行为的加速键：没有对应动作的视图（输入视图）、打开的菜单里一律吞掉 */
export const BROWSER_ACCELERATORS: readonly KeyBinding[] = [
  { combo: 'Mod+KeyR', allowInInput: true, run: swallow },
  { combo: 'Mod+Shift+KeyR', allowInInput: true, run: swallow },
  { combo: 'Mod+KeyP', allowInInput: true, run: swallow },
]

/** 菜单打开期间的额外按键（见模块注释）；浏览器加速键总是一并吞掉 */
export function useMenuKeys(open: boolean, bindings: readonly KeyBinding[] = []): void {
  const latest = useRef(bindings)
  useLayoutEffect(() => {
    latest.current = bindings
  })
  useEffect(() => {
    if (!open) return
    const onKey = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing || event.keyCode === 229) return
      for (const binding of [...latest.current, ...BROWSER_ACCELERATORS]) {
        if (!matchesEvent(parseCombo(binding.combo, boot.os), event)) continue
        if (binding.run(event, { inInput: false, target: event.target }) === false) continue
        event.preventDefault()
        event.stopPropagation()
        return
      }
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [open])
}
