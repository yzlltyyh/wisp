/**
 * 浮层定位（Menu / Popover / Tooltip / Select / ContextMenu 共用）。
 *
 * 面板是一个只有卡片大小的窗口，浮层**不能**画到卡片外面（Native 帧模式下窗口外不可见；Css 帧模式下会落在透明边距里）。
 * 因此面板卡片提供一个 `FloatingBoundary`：
 * - `container`：浮层的挂载点（卡片内一个铺满的层，随卡片一起裁切）；
 * - `rect()`：可用区域 = 卡片顶边起、最大高度（`PanelPrepare.max_height`）为止；
 * - `reserve(id, bottom)`：浮层需要的底边（相对卡片顶边）。卡片内容的最小高度随之增大，卡片按正常的高度跟随长高
 *   （Css：预留引起的长高 / 回收用 spring.snappy；Native：Rust 逐帧弹簧），浮层不会被裁掉；关闭时等浮层淡出
 *   （menuOut）之后再释放——卡片不在浮层淡出的同时缩回；
 * - `edge`：卡片当前**可见的**下沿（相对卡片顶边）。浮层打开就在最终位置播 menuIn，卡片追赶期间按这条下沿裁切
 *   浮层的底部（下拉帘，保留浮层自己的圆角），而不是被卡片的直边一路「擦开」。所有面板菜单都走这里。
 * 定位：优先放在 `side` 一侧；放不下而另一侧放得下（含卡片能长高的空间）时翻过去；两侧都放不下时取空间大的一侧、
 * 内部滚动。`shift`（右键菜单这类以指针为锚的浮层）：两侧都放不下、而边界本身放得下时，整份浮层上移到边界内（盖住
 * 锚点），只有边界比浮层还矮时才限高滚动。
 * 没有 boundary 的窗口（设置 / 引导）挂到 body、以视口为边界。
 */
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
  type RefObject,
} from 'react'
import { createPortal } from 'react-dom'
import { ms, timing } from '../motion/tokens'

/**
 * 一条随时间变化的高度（卡片可见的下沿，相对卡片顶边，px）。Infinity = 卡片高度与内容同步（不需要下拉帘）
 */
export interface VisibleEdge {
  get(): number
  subscribe(listener: () => void): () => void
}

/**
 * VisibleEdge 的实现。`lazy`：有第一个订阅者时开始观察、最后一个退订时停止（返回停止函数）——空闲时（没有浮层打开）
 * 不挂任何观察者
 */
export class EdgeStore implements VisibleEdge {
  private value: number
  private readonly listeners = new Set<() => void>()
  private stop: (() => void) | null = null
  constructor(
    initial = Infinity,
    private readonly lazy?: (store: EdgeStore) => () => void,
  ) {
    this.value = initial
  }
  readonly get = (): number => this.value
  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    if (this.listeners.size === 1 && this.lazy) this.stop = this.lazy(this)
    return () => {
      if (!this.listeners.delete(listener) || this.listeners.size > 0) return
      this.stop?.()
      this.stop = null
    }
  }
  /** 有没有订阅者（没有时调用方可以不推演动画、直接到位） */
  get watched(): boolean {
    return this.listeners.size > 0
  }
  set(v: number): void {
    if (v === this.value) return
    this.value = v
    for (const l of this.listeners) l()
  }
}

export interface FloatingBoundary {
  /** 浮层挂载点；null 时挂到 body */
  readonly container: HTMLElement | null
  /** 可用区域（视口坐标） */
  rect(): DOMRect
  /** 预留卡片内容高度（相对 rect().top 的底边，px）；null 释放 */
  reserve?(id: symbol, bottom: number | null): void
  /** 卡片当前可见的下沿（相对 rect().top）；没有时不裁切（卡片高度与内容同步：Pinned、不透明窗口） */
  readonly edge?: VisibleEdge
}

export const FloatingContext = createContext<FloatingBoundary | null>(null)

export function useFloatingBoundary(): FloatingBoundary | null {
  return useContext(FloatingContext)
}

export type Side = 'top' | 'bottom'
export type Align = 'start' | 'center' | 'end'

export interface PositionOptions {
  side?: Side
  align?: Align
  /** 与锚点的间距 */
  offset?: number
  /** 与边界的最小距离 */
  padding?: number
  /** 空间不够时是否翻到另一侧，默认 true */
  flip?: boolean
  /** 两侧都放不下、边界本身放得下时整份上移到边界内（以指针为锚的右键菜单），默认 false */
  shift?: boolean
}

export interface Position {
  /** 视口坐标 */
  x: number
  y: number
  side: Side
  /** 浮层可用的最大高度（超出时内部滚动） */
  maxHeight: number
  /** 变换原点（相对浮层自身），浮层从触发器一侧展开 */
  origin: string
  /** 浮层底边相对边界顶边的距离（用于 reserve） */
  bottomFromTop: number
}

interface Box {
  left: number
  top: number
  width: number
  height: number
}

/** 纯函数：给定锚点、浮层尺寸与边界，算出位置（单测覆盖） */
export function computePosition(
  anchor: Box,
  size: { width: number; height: number },
  boundary: Box,
  options: PositionOptions = {},
): Position {
  const {
    side = 'bottom',
    align = 'start',
    offset = 4,
    padding = 8,
    flip = true,
    shift = false,
  } = options
  const bTop = boundary.top + padding
  const bBottom = boundary.top + boundary.height - padding
  const bLeft = boundary.left + padding
  const bRight = boundary.left + boundary.width - padding
  const below = bBottom - (anchor.top + anchor.height + offset)
  const above = anchor.top - offset - bTop
  let actual: Side = side
  if (flip) {
    // 放不下时翻到另一侧：另一侧放得下整份（不用滚动），或者两侧都放不下而另一侧空间更大
    const fitsBelow = size.height <= below
    const fitsAbove = size.height <= above
    if (side === 'bottom' && !fitsBelow && (fitsAbove || above > below)) actual = 'top'
    else if (side === 'top' && !fitsAbove && (fitsBelow || below > above)) actual = 'bottom'
  }
  let room = Math.max(0, actual === 'bottom' ? below : above)
  let height = Math.min(size.height, room)
  let y = actual === 'bottom' ? anchor.top + anchor.height + offset : anchor.top - offset - height
  if (shift && size.height > room && size.height <= bBottom - bTop) {
    // 整份上移 / 下移到边界内，尽量贴近锚点（盖住锚点，不在菜单里滚动）
    room = size.height
    height = size.height
    actual = 'bottom'
    y = Math.max(bTop, Math.min(anchor.top + anchor.height + offset, bBottom - height))
  }

  let x =
    align === 'start'
      ? anchor.left
      : align === 'end'
        ? anchor.left + anchor.width - size.width
        : anchor.left + (anchor.width - size.width) / 2
  x = Math.max(bLeft, Math.min(x, bRight - size.width))
  // 变换原点：水平方向取锚点中心相对浮层的位置，垂直方向在触发器一侧
  const anchorCenter = anchor.left + anchor.width / 2
  const originX = Math.max(0, Math.min(size.width, anchorCenter - x))
  return {
    x: Math.round(x),
    y: Math.round(y),
    side: actual,
    maxHeight: Math.floor(room),
    origin: `${Math.round(originX)}px ${actual === 'bottom' ? 'top' : 'bottom'}`,
    bottomFromTop: Math.ceil(y + height - boundary.top + padding),
  }
}

function viewportBox(): Box {
  return { left: 0, top: 0, width: window.innerWidth, height: window.innerHeight }
}

export interface UseFloatingOptions extends PositionOptions {
  open: boolean
  /** 锚点：元素或固定矩形（ContextMenu 的指针位置） */
  anchor: RefObject<HTMLElement | null> | Box | null
  /** 是否预留卡片高度（菜单 / Popover 为 true，Tooltip 为 false） */
  reserve?: boolean
}

export interface FloatingState {
  ref: RefObject<HTMLDivElement | null>
  style: CSSProperties
  /** 已完成定位（之前保持不可见，避免在错误位置闪一帧） */
  placed: boolean
  side: Side
  /** 重新计算（内容尺寸变化后调用） */
  update(): void
}

/**
 * 浮层定位 hook：打开时在布局阶段同步测量并定位；在面板卡片里预留高度、按卡片可见的下沿做下拉帘、关闭淡出之后才
 * 释放预留（见文件头）
 */
export function useFloating(options: UseFloatingOptions): FloatingState {
  const boundary = useFloatingBoundary()
  const ref = useRef<HTMLDivElement | null>(null)
  const id = useRef(Symbol('floating'))
  const [pos, setPos] = useState<(Position & { left: number; top: number }) | null>(null)
  const { open, anchor, side, align, offset, padding, flip, shift, reserve = false } = options
  /** 下拉帘用的几何（相对边界顶边；每次定位时更新，逐帧裁切时不读布局） */
  const geo = useRef<{ top: number; bottom: number; radius: string } | null>(null)
  /** 关闭之后延迟释放预留的计时器 */
  const releaseTimer = useRef(0)

  const update = useCallback(() => {
    const el = ref.current
    if (!el || !anchor) return
    const a: Box =
      'current' in anchor
        ? (anchor.current?.getBoundingClientRect() ?? { left: 0, top: 0, width: 0, height: 0 })
        : anchor
    const b: Box = boundary ? boundary.rect() : viewportBox()
    // 测量自然尺寸：先去掉 maxHeight 限制
    const prevMax = el.style.maxHeight
    el.style.maxHeight = 'none'
    const size = { width: el.offsetWidth, height: el.scrollHeight }
    el.style.maxHeight = prevMax
    const computed = computePosition(a, size, b, { side, align, offset, padding, flip, shift })
    const top = computed.y - b.top
    geo.current = {
      top,
      bottom: top + Math.min(size.height, computed.maxHeight),
      radius: getComputedStyle(el).borderTopLeftRadius || '0px',
    }
    // 挂在卡片内时用 absolute（相对浮层容器），挂在 body 时用 fixed（视口坐标）
    const c = boundary?.container?.getBoundingClientRect()
    const next = { ...computed, left: computed.x - (c?.left ?? 0), top: computed.y - (c?.top ?? 0) }
    setPos((p) =>
      p &&
      p.left === next.left &&
      p.top === next.top &&
      p.maxHeight === next.maxHeight &&
      p.side === next.side &&
      p.origin === next.origin
        ? p
        : next,
    )
    if (reserve && boundary?.reserve) boundary.reserve(id.current, next.bottomFromTop)
  }, [anchor, boundary, side, align, offset, padding, flip, shift, reserve])

  useLayoutEffect(() => {
    // 关闭时保留最后的位置：退出动画仍在原处播放
    if (!open) return
    // 关闭后又很快打开：上一次的预留还没释放，接着用
    window.clearTimeout(releaseTimer.current)
    update()
    // 窗口尺寸变化、任何容器滚动时跟随锚点（每帧最多一次）；只在打开期间监听
    let frame = 0
    const schedule = () => {
      if (!frame) {
        frame = requestAnimationFrame(() => {
          frame = 0
          update()
        })
      }
    }
    window.addEventListener('resize', schedule)
    window.addEventListener('scroll', schedule, { capture: true, passive: true })
    const reserveId = id.current
    return () => {
      window.removeEventListener('resize', schedule)
      window.removeEventListener('scroll', schedule, { capture: true })
      if (frame) cancelAnimationFrame(frame)
      // 等浮层淡出（menuOut）之后再释放：卡片不在浮层淡出的同时缩回（每开一次菜单整块面板「呼吸」一次）
      if (reserve && boundary?.reserve) {
        const release = boundary.reserve
        releaseTimer.current = window.setTimeout(() => {
          releaseTimer.current = 0
          release(reserveId, null)
        }, ms(timing.menuOut))
      }
    }
  }, [open, update, reserve, boundary])

  // 卸载（视图换掉、面板隐藏）：立即释放
  useEffect(() => {
    const reserveId = id.current
    return () => {
      if (!releaseTimer.current) return
      window.clearTimeout(releaseTimer.current)
      releaseTimer.current = 0
      boundary?.reserve?.(reserveId, null)
    }
  }, [boundary])

  // 下拉帘（见文件头）：浮层的下沿超出卡片当前可见的下沿时，按可见下沿裁切浮层底部（round：保留浮层自己的圆角）。
  // 只在打开期间订阅；卡片 / 窗口高度变化时才回调（空闲时没有计时器）。逐帧只做算术：几何在定位时记下，裁切量写成
  // calc(100% - 可见高度)，不读布局。这是动效规范「只动 transform / opacity」的例外（与面板高度同属
  // 「卡片追赶内容」的那一类，最长一次卡片弹簧的时长）
  const edge = reserve ? boundary?.edge : undefined
  const pad = padding ?? 8
  useLayoutEffect(() => {
    if (!edge || !open) return
    let last: string | null = null
    const apply = () => {
      const el = ref.current
      const g = geo.current
      if (!el || !g) return
      const visible = edge.get() - pad
      const clip =
        visible >= g.bottom - 0.5
          ? ''
          : `inset(0 0 max(0px, calc(100% - ${Math.max(0, Math.floor(visible - g.top))}px)) 0 round ${g.radius})`
      if (clip === last) return
      last = clip
      el.style.clipPath = clip
    }
    apply()
    return edge.subscribe(apply)
  }, [edge, open, pad])

  const style: CSSProperties = pos
    ? {
        position: boundary?.container ? 'absolute' : 'fixed',
        left: pos.left,
        top: pos.top,
        maxHeight: pos.maxHeight,
        transformOrigin: pos.origin,
      }
    : { position: 'fixed', left: 0, top: 0, visibility: 'hidden' }
  return { ref, style, placed: pos !== null, side: pos?.side ?? side ?? 'bottom', update }
}

/** 把浮层挂到 boundary 的容器（面板卡片内）或 body */
export function FloatingPortal({ children }: { children: ReactNode }) {
  const boundary = useFloatingBoundary()
  const target = boundary?.container ?? (typeof document !== 'undefined' ? document.body : null)
  if (!target) return null
  return createPortal(children, target)
}
