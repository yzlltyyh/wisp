/**
 * ScrollArea：细滚动条、`scrollbar-gutter: stable`（出现滚动条时内容不跳）、
 * `overscroll-behavior: contain`；可滚动且未到顶 / 底时，在对应边缘显示 12 px 的渐隐遮罩（mask，不重绘内容）。
 *
 * 面板内（Native 帧模式下视口比卡片高）必须给出显式 `maxHeight`（px），不要用 100% / vh。
 * 渐隐只在滚动时、以及容器或内容的尺寸变化时同步：尺寸变化由 ResizeObserver 在布局之后回报（读 scrollTop /
 * scrollHeight 不会强制布局），不在每次渲染的提交阶段读布局。
 */
import { useCallback, useLayoutEffect, useRef, type ReactNode, type Ref, type UIEvent } from 'react'
import { cx } from './cx'

/** 按滚动位置更新边缘渐隐（ScrollArea / VirtualList 共用）；只写有变化的属性 */
export function syncFadeEdges(el: HTMLElement): void {
  const top = el.scrollTop > 1
  const bottom = el.scrollTop + el.clientHeight < el.scrollHeight - 1
  if ((el.dataset.fadeTop === 'true') !== top) el.dataset.fadeTop = String(top)
  if ((el.dataset.fadeBottom === 'true') !== bottom) el.dataset.fadeBottom = String(bottom)
}

export interface ScrollAreaProps {
  children: ReactNode
  maxHeight?: number
  className?: string
  /** 边缘渐隐（默认 true） */
  fade?: boolean
  onScroll?: (e: UIEvent<HTMLDivElement>) => void
  ref?: Ref<HTMLDivElement>
  'data-testid'?: string
  tabIndex?: number
  role?: string
  'aria-label'?: string
}

export function ScrollArea({
  children,
  maxHeight,
  className,
  fade = true,
  onScroll,
  ref,
  'data-testid': testId,
  ...rest
}: ScrollAreaProps) {
  const inner = useRef<HTMLDivElement | null>(null)
  const setRef = useCallback(
    (el: HTMLDivElement | null) => {
      inner.current = el
      if (typeof ref === 'function') ref(el)
      else if (ref) ref.current = el
    },
    [ref],
  )
  const sync = useCallback(() => {
    const el = inner.current
    if (el && fade) syncFadeEdges(el)
  }, [fade])
  // 容器与直接子元素的尺寸变化（内容增减、换行、maxHeight 变化）：布局之后同步，挂载后的第一次回报给出初值
  const observer = useRef<ResizeObserver | null>(null)
  useLayoutEffect(() => {
    if (!fade) return
    const ro = new ResizeObserver(sync)
    observer.current = ro
    return () => {
      ro.disconnect()
      observer.current = null
    }
  }, [fade, sync])
  // 每次提交后补上新增的子元素（observe 同一元素是空操作，不读布局）
  useLayoutEffect(() => {
    const el = inner.current
    const ro = observer.current
    if (!el || !ro) return
    ro.observe(el)
    for (const child of el.children) ro.observe(child)
  })
  return (
    <div
      {...rest}
      ref={setRef}
      className={cx('ui-scroll', className)}
      style={maxHeight !== undefined ? { maxHeight } : undefined}
      data-testid={testId}
      onScroll={(e) => {
        sync()
        onScroll?.(e)
      }}
    >
      {children}
    </div>
  )
}
