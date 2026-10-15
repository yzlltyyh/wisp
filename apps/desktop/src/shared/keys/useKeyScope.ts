/**
 * 视图注册键盘作用域。
 *
 * 绑定数组可以每次渲染都新建：这里用 ref 保存最新值，作用域只在挂载 / scope 名或层级变化时注册一次，
 * 处理函数里读到的永远是最新的 props / state。挂载时自动安装全局路由（引用计数）。
 * 所在面板视图不在显示中（视图切换的暂存层、正在淡出被替换的旧视图，见 shared/hooks/useViewActive）时作用域
 * 自动停用：新视图显示之前不抢键，被替换的旧视图不会在淡出期间再响应 Enter 等按键。
 *
 * ```ts
 * useKeyScope('translate', [
 *   { combo: 'Mod+KeyC', allowInInput: true, run: copy },
 *   { combo: 'Mod+KeyP', allowInInput: true, run: pin },
 * ])
 * ```
 */
import { useEffect, useLayoutEffect, useRef } from 'react'
import { useViewActive } from '../hooks/useViewActive'
import { installKeyRouter, registerScope, type KeyBinding, type ScopeOptions } from './keyRouter'

export function useKeyScope(
  scope: string,
  bindings: readonly KeyBinding[],
  options: ScopeOptions & { enabled?: boolean } = {},
): void {
  const latest = useRef(bindings)
  useLayoutEffect(() => {
    latest.current = bindings
  })
  const { layer = 'view', modal = false } = options
  const viewActive = useViewActive()
  const enabled = (options.enabled ?? true) && viewActive
  useEffect(() => {
    if (!enabled) return
    const uninstall = installKeyRouter()
    const unregister = registerScope(scope, () => latest.current, { layer, modal })
    return () => {
      unregister()
      uninstall()
    }
  }, [scope, layer, modal, enabled])
}
