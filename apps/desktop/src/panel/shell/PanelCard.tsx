/**
 * 面板卡片。两种帧模式：
 *
 * - **native**（macOS）：窗口即卡片。材质（NSVisualEffectView）、着色（`--surface-tint`）与 0.5 pt 描边都由 Rust 的
 *   原生层画在窗口矩形内（`windows/native/macos.rs`），CSS 卡片背景透明；窗口高度由 Rust 逐帧弹簧驱动，进入 / 退出是
 *   窗口 alpha，CSS 只让**内容**在进入时做 y ±6→0（spring.snappy），退出时内容不动；减少动效时内容没有位移，
 *   进入阶段直接视为播完。WebView 被固定为「卡片上限」高、贴住窗口顶边，
 *   窗口只露出其中卡片那一段，所以卡片按内容**自然高度**排版：不要用 `height: 100%`、不要把元素钉在视口底部；
 *   需要内部滚动的视图用 `maxHeight` 给出显式高度。卡片内的组件不要再铺 `--surface-tint` 背景（会与原生着色叠加）。
 * - **css**（Windows / Linux，macOS 退路）：透明窗口里画纯色卡片 + CSS 阴影，四周留 `--shadow-margin`（Rust 注入，
 *   唯一来源 `windows::sizing::SHADOW_MARGIN`）；Below / Center 贴顶、Above 贴底；卡片高度用 spring.gentle 跟随内容
 *   （`useHeightFollow`），进入 / 退出播放 `presets.panelEnter` / `presets.panelExit`（变换原点在锚点一侧）。
 * - **opaque**（Css 帧模式但 `shadowMargin` 为 0，窗口不能透明，如 Wayland）：卡片铺满窗口、实色直角、无阴影；
 *   窗口本身不透明，卡片与外框**不能**做透明度或缩放（露出的是 WebView 的默认底色，WebKitGTK 上是白色，深色模式下
 *   每次开关都会白闪）——进入时只有内容 opacity 0→1（`presets.panelEnterOpaque`），退出不播动画（Rust 直接隐藏）；
 *   卡片高度随窗口（高度变化立即上报，useHeightFollow 的 opaque 方式）。
 *
 * 卡片同时是浮层（菜单 / tooltip / popover）的边界：它们挂在卡片内的浮层层上、随卡片裁切，需要时预留内容高度让卡片长高
 * （shared/ui/floating.tsx），并提供卡片当前**可见的**下沿（`edge`）给菜单做下拉帘：Css 帧读卡片的布局高度（有菜单
 * 打开时才观察）；Native 帧用外壳按 Rust 同参弹簧推演的窗口高度（useHeightFollow）；mock 的 Native 帧读模拟窗口；
 * 不透明窗口不需要（卡片高度立即生效）。预留引起的长高与关闭后的回收用 spring.snappy（约 230 ms），比内容
 * 变化的 spring.gentle 快——菜单按下就出现，卡片尽快让出位置：Css 帧是卡片高度的 Motion 弹簧，
 * Native 帧由高度跟随在 `panel_resize` 里指定、Rust 的窗口弹簧随之换参数——两种帧同一条规则
 * （`quickResize`），预留状态经 `resizeHint` 交给高度跟随。
 * 卡片设置 `contain: layout paint`，高度动画不会触发外层回流。
 *
 * 浏览器 mock 下没有原生层：Native 帧模式额外画一层模拟的着色与描边（只在 mock 中，生产包里整段删除）。
 */
import { m } from 'motion/react'
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
  type Ref,
} from 'react'
import { getTransport } from '../../shared/ipc'
import type { FrameMode, Placement } from '../../shared/ipc/types'
import { adaptPreset, presets } from '../../shared/motion/presets'
import { spring } from '../../shared/motion/tokens'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import {
  EdgeStore,
  FloatingContext,
  type FloatingBoundary,
  type VisibleEdge,
} from '../../shared/ui/floating'
import type { PanelPhase } from '../types'
import { quickResize, RELEASE_WINDOW_MS } from './useHeightFollow'

export interface PanelCardProps {
  frame: FrameMode
  phase: PanelPhase
  /** 卡片宽度（逻辑 px） */
  width: number
  /** 卡片上限高度（PanelPrepare.max_height）：浮层可用区域 */
  maxHeight: number
  placement: Placement
  /** 锚点 x 相对卡片左边的偏移（Css 帧模式的 transform-origin） */
  originX: number
  /** Css 帧模式的阴影边距（Boot.shadow_margin）；0 表示窗口不能透明 */
  shadowMargin: number
  /** Css 帧模式：卡片高度（null = 跟随内容的自然高度，prepare 阶段） */
  height: number | null
  /** Native 帧模式：窗口可见下沿的推演（useHeightFollow）；其余模式不使用 */
  windowEdge?: VisibleEdge
  contentRef: Ref<HTMLDivElement>
  /** Css 帧模式：卡片高度动画结束 */
  onHeightSettled?: () => void
  /** 进入动画结束 */
  onEntered?: () => void
  /** Css 帧模式：退出动画播完（Native 帧模式的退出是窗口 alpha，不回调） */
  onExited?: () => void
  /** 叠在卡片最上层、相对卡片定位的装饰（frame lab 标记） */
  overlay?: ReactNode
  /**
   * 内容之后、参与布局的一行（开发者模式的延迟 / 帧监视）：计入内容高度，不盖住视图底栏；
   * 外壳给视图的 maxHeight 已经扣掉了它的高度
   */
  footer?: ReactNode
  /** Native 帧模式下改由 CSS 画着色与描边（frame lab 的对照组；正式路径为 false） */
  cssFill?: boolean
  /** 浮层预留的内容高度变化（提交阶段回调；外壳据此维护交给高度跟随的 `ResizeHint`） */
  onReserveChange?: (reserved: number) => void
  children: ReactNode
}

export function PanelCard(props: PanelCardProps) {
  // 卡片元素（交给三种卡片的 ref）。用普通的可变对象而不是 useRef：卡片可见下沿的观察者在初始化时就要拿到它（有订阅者
  // 时才读 current），渲染期间从不读
  const [card] = useState<{ current: HTMLDivElement | null }>(() => ({ current: null }))
  const [layer, setLayer] = useState<HTMLDivElement | null>(null)
  const reserves = useRef(new Map<symbol, number>())
  const [reserved, setReserved] = useState(0)
  const { maxHeight, frame, shadowMargin, windowEdge, onReserveChange } = props
  // 预留状态交给高度跟随（提交阶段回调：预留改变 → 内容的 min-height 改变 → ResizeObserver → 下一帧上报，读到的已是新值）
  useLayoutEffect(() => {
    onReserveChange?.(reserved)
  }, [reserved, onReserveChange])
  // 卡片可见的下沿（见文件头）：Css 帧观察卡片、mock 的 Native 帧观察模拟窗口——都只在有菜单打开（有订阅者）时观察
  const [cardEdge] = useState(() => new EdgeStore(Infinity, observeHeight(card)))
  const [rootEdge] = useState(() => new EdgeStore(Infinity, observeHeight(ROOT)))
  const edge =
    frame === 'native'
      ? emulateNativeSurface()
        ? rootEdge
        : windowEdge
      : shadowMargin === 0
        ? undefined
        : cardEdge
  const reserve = useCallback(
    (id: symbol, bottom: number | null) => {
      if (bottom === null) reserves.current.delete(id)
      else reserves.current.set(id, Math.min(bottom, maxHeight))
      let max = 0
      for (const v of reserves.current.values()) max = Math.max(max, v)
      setReserved(max)
    },
    [maxHeight],
  )
  const boundary = useMemo<FloatingBoundary>(
    () => ({
      container: layer,
      rect: () => {
        const r = card.current?.getBoundingClientRect()
        return new DOMRect(r?.left ?? 0, r?.top ?? 0, r?.width ?? 0, maxHeight)
      },
      reserve,
      edge,
    }),
    [layer, maxHeight, reserve, edge, card],
  )
  // i18n-ignore：下一行之后的 return 不是文案（检查器按「标签之后的单词」判断）
  const floatingLayer = <div ref={setLayer} className="panel-floating" />
  return (
    <FloatingContext value={boundary}>
      {props.frame === 'native' ? (
        <NativeCard {...props} cardRef={card} reserved={reserved} floating={floatingLayer} />
      ) : props.shadowMargin === 0 ? (
        <OpaqueCard {...props} cardRef={card} reserved={reserved} floating={floatingLayer} />
      ) : (
        <CssCard {...props} cardRef={card} reserved={reserved} floating={floatingLayer} />
      )}
    </FloatingContext>
  )
}

/** mock 的模拟窗口（#root）：Native 帧模式下它就是窗口 */
const ROOT = {
  get current(): HTMLElement | null {
    return document.getElementById('root')
  },
}

/** 有订阅者时观察元素的布局高度（ResizeObserver，尺寸变化时才回调），返回停止函数 */
function observeHeight(target: { readonly current: HTMLElement | null }) {
  return (store: EdgeStore) => {
    const el = target.current
    if (!el || typeof ResizeObserver === 'undefined') return () => undefined
    const read = () => store.set(el.id === 'root' ? el.clientHeight : el.offsetHeight)
    read()
    const ro = new ResizeObserver(read)
    ro.observe(el)
    return () => ro.disconnect()
  }
}

interface InnerProps extends PanelCardProps {
  cardRef: Ref<HTMLDivElement>
  /** 浮层预留的内容最小高度 */
  reserved: number
  floating: ReactNode
}

/**
 * mock 下模拟原生着色层（生产包里 __WISP_MOCK__ 为 false，整段删除）。画在卡片之外、铺满模拟窗口（#root），
 * 与真实的原生层一样跟随窗口而不是卡片：卡片设了 contain，不能把它放进卡片里。
 */
function emulateNativeSurface(): boolean {
  return __WISP_MOCK__ && getTransport().kind === 'mock'
}

function NativeCard({
  phase,
  placement,
  originX,
  contentRef,
  onEntered,
  overlay,
  footer,
  cssFill = false,
  cardRef,
  reserved,
  floating,
  children,
}: InnerProps) {
  const reduced = useReducedMotionPref()
  const enter = presets.panelEnter(placement, originX, 'native')
  // 只有 prepare 阶段停在起始位移；退出只靠窗口 alpha，内容原地不动，避免淡出时往回滑的残影
  const target = phase === 'preparing' && !reduced ? enter.initial : enter.animate
  // 减少动效：内容没有位移动画、Motion 不会回调完成，进入阶段直接视为播完（否则停在 entering、到不了 shown）
  useLayoutEffect(() => {
    if (reduced && phase === 'entering') onEntered?.()
  }, [reduced, phase, onEntered])
  return (
    <>
      {emulateNativeSurface() && !cssFill ? (
        <div className="panel-native-surface" aria-hidden="true" />
      ) : null}
      <div
        ref={cardRef}
        className="panel-card"
        data-testid="panel-card"
        data-phase={phase}
        data-frame="native"
        data-css-fill={cssFill || undefined}
      >
        <m.div
          ref={contentRef}
          className="panel-content"
          data-panel-content
          initial={reduced ? false : enter.initial}
          animate={target}
          transition={reduced ? { duration: 0 } : enter.transition}
          onAnimationComplete={() => {
            if (!reduced && phase === 'entering') onEntered?.()
          }}
          style={reserved > 0 ? { minHeight: reserved } : undefined}
        >
          {children}
          {footer}
        </m.div>
        {floating}
        {overlay}
      </div>
    </>
  )
}

function CssCard({
  phase,
  width,
  placement,
  originX,
  height,
  contentRef,
  onHeightSettled,
  onEntered,
  onExited,
  overlay,
  footer,
  cardRef,
  reserved,
  floating,
  children,
}: InnerProps) {
  const reduced = useReducedMotionPref()
  const raw = presets.panelEnter(placement, originX, 'css')
  const enter = adaptPreset(raw, 'fade', reduced)
  // 预留引起的长高（卡片目标就是预留的高度）与释放预留之后紧接着的那一次收缩（内容的 ResizeObserver → 下一帧上报，
  // 通常 1–2 帧之内）：spring.snappy（见文件头）。释放之后内容本来就比预留高、高度没变时，过了这个窗口就不再算
  const [seenReserved, setSeenReserved] = useState(reserved)
  const [releasing, setReleasing] = useState(false)
  if (seenReserved !== reserved) {
    setSeenReserved(reserved)
    setReleasing(seenReserved > 0 && reserved === 0)
  }
  useEffect(() => {
    if (!releasing) return
    const timer = setTimeout(() => setReleasing(false), RELEASE_WINDOW_MS)
    return () => clearTimeout(timer)
  }, [releasing])
  const quick = quickResize(reserved, height, releasing)
  const heightSpring = reduced ? { duration: 0 } : quick ? spring.snappy : spring.gentle
  const exit = reduced ? { opacity: 0, transition: presets.reduced.fade } : presets.panelExit
  // Motion 的完成回调里读最新阶段（回调闭包可能早于阶段切换创建）
  const phaseRef = useRef(phase)
  useLayoutEffect(() => {
    phaseRef.current = phase
  }, [phase])
  const target =
    phase === 'exiting'
      ? exit
      : phase === 'entering' || phase === 'shown'
        ? { ...enter.animate, transition: enter.transition }
        : enter.initial || undefined
  return (
    <m.div
      className="panel-frame"
      data-testid="panel-card-frame"
      data-placement={placement}
      initial={enter.initial}
      animate={target}
      onAnimationComplete={() => {
        if (phaseRef.current === 'exiting') onExited?.()
        else if (phaseRef.current === 'entering') onEntered?.()
      }}
      style={{ width, transformOrigin: raw.transformOrigin }}
    >
      <m.div
        ref={cardRef}
        className="panel-card"
        data-testid="panel-card"
        data-phase={phase}
        data-frame="css"
        initial={false}
        animate={{ height: height ?? 'auto' }}
        transition={heightSpring}
        onAnimationComplete={onHeightSettled}
      >
        <div
          ref={contentRef}
          className="panel-content"
          data-panel-content
          style={reserved > 0 ? { minHeight: reserved } : undefined}
        >
          {children}
          {footer}
        </div>
        {floating}
        {overlay}
      </m.div>
    </m.div>
  )
}

/**
 * 窗口不能透明（Css 帧模式且 shadow_margin = 0）：外框铺满窗口、不做任何变换或透明度；卡片实色、直角，高度随窗口。
 * 卡片底色从第一帧起就是实色，进入时只让内容淡入；退出不播动画（usePanelLifecycle 立即 panel_hide，Rust 直接隐藏，
 * 内容在 fast 之后才卸载），所以这里没有 onExited。
 */
function OpaqueCard({
  phase,
  placement,
  contentRef,
  onEntered,
  overlay,
  footer,
  cardRef,
  reserved,
  floating,
  children,
}: InnerProps) {
  const reduced = useReducedMotionPref()
  const enter = adaptPreset(presets.panelEnterOpaque, 'fade', reduced)
  const phaseRef = useRef(phase)
  useLayoutEffect(() => {
    phaseRef.current = phase
  }, [phase])
  return (
    <div
      className="panel-frame"
      data-testid="panel-card-frame"
      data-placement={placement}
      data-opaque
    >
      <div
        ref={cardRef}
        className="panel-card"
        data-testid="panel-card"
        data-phase={phase}
        data-frame="css"
      >
        <m.div
          ref={contentRef}
          className="panel-content"
          data-panel-content
          initial={enter.initial}
          animate={phase === 'preparing' ? enter.initial || undefined : enter.animate}
          transition={enter.transition}
          onAnimationComplete={() => {
            if (phaseRef.current === 'entering') onEntered?.()
          }}
          style={reserved > 0 ? { minHeight: reserved } : undefined}
        >
          {children}
          {footer}
        </m.div>
        {floating}
        {overlay}
      </div>
    </div>
  )
}
