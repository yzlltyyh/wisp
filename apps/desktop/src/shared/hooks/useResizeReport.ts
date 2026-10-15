/**
 * 观察元素高度并上报。Pinned 窗口、展开区等需要把内容高度告诉 Rust 的地方使用。
 *
 * - 只在窗口可见后挂载 ResizeObserver（`enabled` 为 false 或 `useHidden()` 为 true 时断开），prepare 路径不依赖它；
 * - 同一帧内的多次变化合并为一次（rAF），只在取整后的高度变化时回调；
 * - 测量值不是有限非负数时不上报（Rust 会以 invalid_argument 拒绝）。
 */
import { useEffect, useLayoutEffect, useRef, type RefObject } from 'react'
import { useHidden } from './useHidden'

export function useResizeReport(
  ref: RefObject<HTMLElement | null>,
  onHeight: (height: number) => void,
  enabled = true,
): void {
  const hidden = useHidden()
  const callback = useRef(onHeight)
  useLayoutEffect(() => {
    callback.current = onHeight
  })
  useEffect(() => {
    const el = ref.current
    if (!el || !enabled || hidden) return
    let frame = 0
    let last = -1
    const measure = () => {
      frame = 0
      // 布局高度（不受 transform 缩放影响）
      const h = el.offsetHeight
      if (!Number.isFinite(h) || h < 0 || h === last) return
      last = h
      callback.current(h)
    }
    const observer = new ResizeObserver(() => {
      if (!frame) frame = requestAnimationFrame(measure)
    })
    observer.observe(el)
    return () => {
      observer.disconnect()
      if (frame) cancelAnimationFrame(frame)
    }
  }, [ref, enabled, hidden])
}
