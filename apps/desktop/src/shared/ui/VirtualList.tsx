/**
 * VirtualList：`@tanstack/react-virtual` 的封装，剪贴板列表等长列表使用。
 *
 * - 混合行高（44 / 64）由 `estimateSize` 给出，行渲染后按实测修正。实测只来自 ResizeObserver（布局之后回报，
 *   不强制布局）：挂载时（ref 回调里）不读 offsetHeight、视口尺寸直接取 `height`（第一次渲染就按它算出首屏行，
 *   不会在布局阶段再同步重渲染一次）、新建的滚动容器不调用 scrollTo(0)，提交阶段就不会被逼着同步排版整张列表；
 * - **选中指示器**放在滚动内容层里（随内容滚动，天然与行同步），`y` / `height` 取 virtualizer 的 `start` / `size`，
 *   都用 spring.snappy 过渡；减少动效时直接跳到目标；指示器不参与 stagger；
 * - 打开时前 8 行做 `rowIn` + `listStagger`（`stagger`，只在挂载后第一帧之前出现的行；减少动效时、以及挂载在视图
 *   切换的暂存层里时没有——整个视图作为一体淡入）；动画期间键盘不阻塞；
 * - 挂载时只渲染视口内的行：面板 prepare 是同步路径（40 ms 内 panel_ready），暂存层里的视图没人滚动，
 *   overscan 的行只会加重这一次的渲染与排版；视图显示后的下一帧再在后台（transition）补上 overscan 的行。
 *   只有这几行包一层 Motion 组件，其余行（以及之后滚动进来的行）是普通 div：滚动热路径上不创建 VisualElement /
 *   MotionValue（滚动期间不能出现 > 8 ms 的长任务）。每行在挂载时决定用哪种，之后不变（元素类型不变，不会重新挂载内容）；
 * - 边缘渐隐只在滚动、总高度或视口高度变化时同步（不在每次渲染时读布局；暂存中不读，显示时再同步）；
 * - 隐藏时外壳会整体卸载视图，这里不需要额外的隐藏处理。
 *
 * 通过 `ref` 拿到 `scrollToIndex`（键盘移动选中项时让它保持可见）。
 */
import { elementScroll, useVirtualizer, type Rect, type Virtualizer } from '@tanstack/react-virtual'
import { m } from 'motion/react'
import {
  startTransition,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
  type Ref,
} from 'react'
import { useViewActive } from '../hooks/useViewActive'
import { presets } from '../motion/presets'
import { spring } from '../motion/tokens'
import { useReducedMotionPref } from '../motion/useReducedMotionPref'
import { cx } from './cx'
import { syncFadeEdges } from './ScrollArea'

/**
 * 视口尺寸：挂载时直接用 props 给出的高度（显式 px），不读 offsetWidth / offsetHeight；之后的变化由 ResizeObserver
 * 在布局之后回报。竖向列表只用高度，宽度在第一次回报之前记为 0
 */
function observeViewport(height: number) {
  return (instance: Virtualizer<HTMLDivElement, Element>, cb: (rect: Rect) => void) => {
    const el = instance.scrollElement
    if (!el) return
    cb({ width: 0, height })
    const observer = new ResizeObserver((entries) => {
      const box = entries[0]?.borderBoxSize?.[0]
      cb(
        box
          ? { width: Math.round(box.inlineSize), height: Math.round(box.blockSize) }
          : { width: el.offsetWidth, height: el.offsetHeight },
      )
    })
    observer.observe(el, { box: 'border-box' })
    return () => observer.disconnect()
  }
}

/**
 * 行高实测：只用 ResizeObserver 的 entry（布局之后的尺寸）；挂载时的 ref 回调没有 entry，沿用已测得或估计的值，
 * 不读 offsetHeight——否则提交阶段就会强制同步布局（新挂载的整张列表），挂载与排版挤进同一个长任务
 */
function measureRow(
  el: Element,
  entry: ResizeObserverEntry | undefined,
  instance: Virtualizer<HTMLDivElement, Element>,
): number {
  const box = entry?.borderBoxSize?.[0]
  if (box) return Math.round(box.blockSize)
  const index = instance.indexFromElement(el)
  const key = instance.options.getItemKey(index)
  return (
    instance.itemSizeCache.get(key) ??
    instance.measurementsCache[index]?.size ??
    instance.options.estimateSize(index)
  )
}

/**
 * 键盘导航的焦点通常留在搜索框上：由视图在搜索框上设置 aria-activedescendant 指向选中行的 id。
 */
export interface VirtualListHandle {
  scrollToIndex(index: number, align?: 'auto' | 'start' | 'center' | 'end'): void
  /** 滚动容器 */
  element(): HTMLDivElement | null
}

export interface VirtualListProps {
  count: number
  /** 每行的估计高度（px） */
  estimateSize: (index: number) => number
  renderItem: (index: number) => ReactNode
  getItemKey?: (index: number) => string | number
  /** 滚动区的显式高度（面板内必须是 px，见 ScrollArea 说明） */
  height: number
  /** 选中项下标（-1 表示没有）；不可选中的行（分组标题）由调用方跳过 */
  selectedIndex?: number
  /** 首次挂载时前 8 行做 stagger 进入 */
  stagger?: boolean
  overscan?: number
  /** 行与行之间、首尾的留白（px） */
  paddingY?: number
  /**
   * 末尾额外的留白（px）：列表底部有浮层盖住一截时（剪贴板的芯片条），最后一行能滚到浮层上方，按键移动时也不会停在
   * 浮层下面
   */
  endInset?: number
  className?: string
  /** 滚动容器（listbox）的 id：组合框的 aria-controls 指向它 */
  id?: string
  role?: string
  'aria-label'?: string
  ref?: Ref<VirtualListHandle>
  'data-testid'?: string
}

export function VirtualList({
  count,
  estimateSize,
  renderItem,
  getItemKey,
  height,
  selectedIndex = -1,
  stagger = false,
  overscan = 6,
  paddingY = 4,
  endInset = 0,
  className,
  id: listId,
  role = 'listbox',
  'aria-label': ariaLabel,
  ref,
  'data-testid': testId = 'virtual-list',
}: VirtualListProps) {
  const reduced = useReducedMotionPref()
  const active = useViewActive()
  // 挂载在视图切换的暂存层里：没有「打开」的 stagger（行是普通 div，提交更轻）
  const [mountedActive] = useState(active)
  // 挂载时只渲染视口内的行；视图显示后的下一帧在后台补上 overscan（prepare 时窗口隐藏、rAF 暂停，显示后才补）
  const [fullOverscan, setFullOverscan] = useState(false)
  useEffect(() => {
    if (!active || fullOverscan) return
    const id = requestAnimationFrame(() => startTransition(() => setFullOverscan(true)))
    return () => cancelAnimationFrame(id)
  }, [active, fullOverscan])
  const scroller = useRef<HTMLDivElement>(null)
  /** 滚动容器是否离开过顶部（用户滚动或程序滚动过）：之前把滚动位置设为 0 是空操作 */
  const scrolled = useRef(false)
  // 「打开」期间：挂载后的第一帧之前。virtualizer 在布局阶段量到视口后会同步再渲染一次（首屏的大部分行在这一次才
  // 出现），所以不能用「首次提交」判断，而是以第一帧为界；面板 prepare 时窗口隐藏、rAF 暂停，显示后才结束
  const opening = useRef(true)
  useEffect(() => {
    if (!opening.current) return
    const id = requestAnimationFrame(() => {
      opening.current = false
    })
    return () => cancelAnimationFrame(id)
  }, [])
  // TanStack Virtual 返回的函数不能被 React Compiler 记忆化；本组件不依赖编译器的 memo，每次渲染直接取最新的虚拟项
  // oxlint-disable-next-line react/incompatible-library
  const virtualizer = useVirtualizer({
    count,
    getScrollElement: () => scroller.current,
    estimateSize,
    getItemKey,
    overscan: fullOverscan ? overscan : 0,
    paddingStart: paddingY,
    paddingEnd: paddingY + endInset,
    scrollPaddingEnd: endInset,
    measureElement: measureRow,
    initialRect: { width: 0, height },
    observeElementRect: observeViewport(height),
    // 挂载时 TanStack 会把滚动位置同步为 0：新建的容器本来就在 0，scrollTo 只会强制同步布局，跳过
    scrollToFn: (offset, options, instance) => {
      if (!scrolled.current && offset + (options.adjustments ?? 0) === 0) return
      scrolled.current = true
      elementScroll(offset, options, instance)
    },
  })
  useImperativeHandle(
    ref,
    () => ({
      scrollToIndex: (index, align = 'auto') => virtualizer.scrollToIndex(index, { align }),
      element: () => scroller.current,
    }),
    [virtualizer],
  )
  const items = virtualizer.getVirtualItems()
  const selected =
    selectedIndex >= 0 ? (virtualizer.measurementsCache[selectedIndex] ?? null) : null
  // 只有打开时的那一批行做 stagger；之后滚动进来的行、插入的行都不再动画
  const animateIn = stagger && !reduced && mountedActive && opening.current
  const max = presets.listStagger.maxItems
  const totalSize = virtualizer.getTotalSize()
  // 读布局：暂存中跳过（不在挂载的提交里强制排版），显示的那次提交里再同步
  useLayoutEffect(() => {
    if (active && scroller.current) syncFadeEdges(scroller.current)
  }, [totalSize, height, active])
  const innerStyle: CSSProperties = { height: totalSize }
  return (
    <div
      ref={scroller}
      className={cx('ui-scroll ui-vlist', className)}
      style={{ height }}
      id={listId}
      role={role}
      aria-label={ariaLabel}
      data-testid={testId}
      onScroll={(e) => {
        scrolled.current = true
        syncFadeEdges(e.currentTarget)
      }}
    >
      <div className="ui-vlist-inner" style={innerStyle}>
        {selected ? (
          <m.div
            className="ui-vlist-indicator"
            aria-hidden="true"
            initial={false}
            animate={{ y: selected.start, height: selected.size }}
            transition={reduced ? { duration: 0 } : spring.snappy}
          />
        ) : null}
        {items.map((item) => (
          <div
            key={item.key}
            data-index={item.index}
            ref={virtualizer.measureElement}
            className="ui-vlist-row"
            style={{ transform: `translateY(${item.start}px)` }}
          >
            <RowContent enter={animateIn && item.index < max} index={item.index}>
              {renderItem(item.index)}
            </RowContent>
          </div>
        ))}
      </div>
    </div>
  )
}

/**
 * 行内容的包装：挂载时决定是否做 stagger 进入，之后不变（useState 的初值），元素类型稳定、内容不会因此重新挂载。
 * 不做动画的行是普通 div，DOM 结构与动画行相同。
 */
function RowContent({
  enter,
  index,
  children,
}: {
  enter: boolean
  index: number
  children: ReactNode
}) {
  const [animated] = useState(enter)
  if (!animated) return <div>{children}</div>
  return (
    <m.div
      initial={presets.rowIn.initial}
      animate={presets.rowIn.animate}
      transition={{
        ...presets.rowIn.transition,
        delay: index * presets.listStagger.staggerChildren,
      }}
    >
      {children}
    </m.div>
  )
}
