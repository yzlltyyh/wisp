/**
 * 视图切换（前端部分）。
 *
 * 新视图先在**暂存层**里渲染：不参与布局（绝对定位）、`inert`、不可见，按目标宽度排版。暂存分步进行，每步各占
 * 一个任务 / 一帧，不让「挂载 + 样式 + 布局 + 绘制」挤进同一个长任务：
 *   1. transition 里渲染并提交（React 并发渲染，按时间片让出主线程）；暂存层先是 `content-visibility: hidden`，
 *      提交之后紧跟的那一帧不会顺带排版它；
 *   2. 下一个任务（`data-laid`）：解除跳过并读一次布局，在自己的任务里完成样式与布局（`visibility: hidden`，不绘制）；
 *   3. 下一帧：浏览器只剩绘制属性的更新（仍不绘制）；
 *   4. 再下一帧（`data-primed`）：改为 opacity 0 的独立合成层，浏览器绘制它（图片在这时解码），屏幕上仍看不见；
 *   5. 再下一帧开始时就绪，可以交换。
 * 交换的那一帧只提交「放进布局 + 改宽 + 开始淡入」，排版与绘制结果都已在缓存里——新视图的挂载、首次布局与
 * 首次绘制都不会落在交叉淡入中间，切换过程不掉帧（流畅度场景 `shell.view-switch`）。
 *
 * - 宽度不变：暂存好之后，旧视图脱离布局（绝对定位，原地淡出）、新视图放进布局并淡入，交叉 120 ms
 *   （presets.crossfade）；卡片高度随新内容由高度跟随过渡（Css：spring.gentle；Native：Rust 逐帧弹簧）；
 * - 宽度要变：旧视图立即在原位淡出（仍在布局里，窗口不动），同时暂存新视图；两者都完成——内容完全透明——
 *   之后在同一次提交里放进新视图、让新宽度生效（`onSwap` → panelStore.commitWidth），新视图随后淡入，
 *   不会出现文字在旧宽度里重排的帧；
 * - 交换提交之后外壳立即上报新内容的高度（`onSwapped` → useHeightFollow.flush），不等 ResizeObserver 的下一帧；
 * - 位置也要变的切换由 Rust 以 `transition = enter` 的 prepare 发起（先退出再进入，卡片重新挂载），不经过这里；
 * - 会话的第一个视图同步渲染（prepare 路径）、不做淡入（卡片的进入动效已经覆盖）；减少动效时交叉淡入 ≤ 100 ms；
 * - 暂存中的视图与已被替换的旧视图（从切换请求的那一刻起，包括宽度不变时暂存期间仍显示着的旧视图）都是 `inert`，
 *   `useViewActive()` 为 false：拿不到焦点、不响应指针、键盘作用域停用——新视图不提前抢键，旧视图不会在淡出期间
 *   再响应 Enter；自动聚焦用 `useAutoFocus`，视图显示时才聚焦；暂存挂载时视图里 `m.*` 的进入动画被屏蔽（整个视图
 *   作为一体淡入），显示之后挂载的照常播放；
 * - 不再是最新的视图（淡出中）冻结在切换前最后一次渲染的结果（`Frozen`）：会话、载荷不会被新视图的值覆盖，
 *   子树也不再随外壳重渲染。
 */
import { m, PresenceContext } from 'motion/react'
import {
  memo,
  startTransition,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ContextType,
  type ReactNode,
} from 'react'
import { flushSync } from 'react-dom'
import { createViewActivity, ViewActivityContext } from '../../shared/hooks/useViewActive'
import type { PanelPayload, PanelView } from '../../shared/ipc/types'
import { presets } from '../../shared/motion/presets'
import { ms, timing } from '../../shared/motion/tokens'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import type { ViewProps } from '../types'
import { views } from '../views'

export type ViewSwitchProps = Omit<ViewProps, 'payload'> & {
  payload: PanelPayload
  /** 视图身份（panelStore.viewKey）：变化即切换 */
  viewKey: number
  /** 这次切换要改宽度（panelStore.nextWidth 不为 null）：旧视图先原地淡出，透明后再改宽、淡入新视图 */
  widthPending: boolean
  /**
   * 暂存层相对当前内容宽度的增量（px）。Css 帧模式 = 新宽度 − 当前宽度（卡片的 CSS 宽度在交换时才变）；
   * Native 帧模式与不透明窗口为 0（内容宽度由窗口决定）
   */
  stageWidthDelta: number
  /** 交换的那次提交里同步调用：外壳在这里让等待中的新宽度生效 */
  onSwap: () => void
  /** 交换提交之后调用：外壳立即上报新内容的高度 */
  onSwapped: () => void
  /** 替换注册表里的视图（mock 视觉稿 design.* 用；生产路径为 undefined） */
  render?: (props: ViewSwitchRenderProps) => ReactNode
}

export type ViewSwitchRenderProps = Omit<
  ViewSwitchProps,
  'render' | 'viewKey' | 'widthPending' | 'stageWidthDelta' | 'onSwap' | 'onSwapped'
>

/** 视图层的状态：显示中 / 宽度变化时在布局里原地淡出 / 宽度不变时脱离布局淡出 / 暂存 */
type LayerState = 'shown' | 'leaving' | 'exiting' | 'staged'

interface Layers {
  /** 在布局里的视图（显示中，或宽度变化时正在原地淡出） */
  current: number
  /** 暂存中的新视图（只会是最新的 viewKey） */
  staged: number | null
  /** 宽度不变的切换：脱离布局、正在淡出的旧视图 */
  exiting: number | null
}

export function ViewSwitch({
  viewKey,
  widthPending,
  stageWidthDelta,
  onSwap,
  onSwapped,
  render,
  ...props
}: ViewSwitchProps) {
  const reduced = useReducedMotionPref()
  const transition = reduced ? presets.reduced.fade : presets.crossfade
  const fadeSeconds = reduced ? timing.reducedFade : timing.crossfade
  const [layers, setLayers] = useState<Layers>(() => ({
    current: viewKey,
    staged: null,
    exiting: null,
  }))

  const {
    sessionId,
    payload,
    phase,
    focused,
    host,
    frame,
    placement,
    maxHeight,
    requestHide,
    switchView,
    activate,
  } = props
  // 最新视图的元素按值记忆：外壳因无关状态（例如交换时改宽）重渲染时，视图子树不跟着重渲染
  const live = useMemo(() => {
    const p: ViewSwitchRenderProps = {
      sessionId,
      payload,
      phase,
      focused,
      host,
      frame,
      placement,
      maxHeight,
      requestHide,
      switchView,
      activate,
    }
    return render ? render(p) : renderView(p)
  }, [
    render,
    sessionId,
    payload,
    phase,
    focused,
    host,
    frame,
    placement,
    maxHeight,
    requestHide,
    switchView,
    activate,
  ])

  const switching = viewKey !== layers.current
  const items: Array<{ key: number; state: LayerState }> = []
  if (layers.exiting !== null) items.push({ key: layers.exiting, state: 'exiting' })
  items.push({ key: layers.current, state: switching && widthPending ? 'leaving' : 'shown' })
  if (switching && layers.staged === viewKey) items.push({ key: viewKey, state: 'staged' })

  // 有新视图要切换且还没暂存：在 transition 里渲染暂存层（宽度变化时旧视图的淡出已在这次提交里开始）
  const stagedKey = layers.staged
  useLayoutEffect(() => {
    if (!switching || stagedKey === viewKey) return
    startTransition(() =>
      setLayers((l) =>
        l.current === viewKey || l.staged === viewKey ? l : { ...l, staged: viewKey },
      ),
    )
  }, [switching, stagedKey, viewKey])

  /** 已完成暂存布局的视图、已在原位淡出完的视图（只用于判断能否交换，不参与渲染） */
  const ready = useRef<number | null>(null)
  const faded = useRef<number | null>(null)
  const latest = useRef({ layers, viewKey, widthPending, onSwap, onSwapped })
  useLayoutEffect(() => {
    latest.current = { layers, viewKey, widthPending, onSwap, onSwapped }
  })

  /** 条件齐备时交换：新视图放进布局并开始淡入，旧视图脱离布局淡出（宽度不变）或移除（宽度变化，已透明） */
  const trySwap = useCallback(() => {
    const {
      layers: l,
      viewKey: key,
      widthPending: wait,
      onSwap: swap,
      onSwapped: swapped,
    } = latest.current
    if (l.staged !== key || l.current === key || ready.current !== key) return
    if (wait && faded.current !== l.current) return
    // 本地状态与外壳的改宽必须在同一次提交里生效（store 的更新与本地 setState 默认不在同一条车道）
    flushSync(() => {
      setLayers((s) =>
        s.staged === key ? { current: key, staged: null, exiting: wait ? null : s.current } : s,
      )
      swap()
    })
    swapped()
  }, [])

  const onStaged = useCallback(
    (key: number) => {
      ready.current = key
      trySwap()
    },
    [trySwap],
  )
  const onFaded = useCallback(
    (key: number) => {
      faded.current = key
      trySwap()
    },
    [trySwap],
  )
  const onExited = useCallback((key: number) => {
    setLayers((l) => (l.exiting === key ? { ...l, exiting: null } : l))
  }, [])

  const stageWidth = stageWidthDelta ? `calc(100% + ${stageWidthDelta}px)` : '100%'
  return (
    <div className="panel-viewswitch">
      {items.map(({ key, state }) => (
        <ViewLayer
          key={key}
          id={key}
          state={state}
          kind={payload.kind}
          retiring={key !== viewKey && state !== 'staged'}
          stageWidth={stageWidth}
          transition={transition}
          fadeSeconds={fadeSeconds}
          onStaged={onStaged}
          onFaded={onFaded}
          onExited={onExited}
        >
          {/* 不是最新的视图：冻结，忽略这里传入的值（最新载荷属于别的视图） */}
          <Frozen frozen={key !== viewKey}>{key === viewKey ? live : null}</Frozen>
        </ViewLayer>
      ))}
    </div>
  )
}

/**
 * 冻结：`frozen` 时跳过重渲染，保留上一次渲染的结果（React.memo 的比较函数恒判相等）。视图一旦不是最新的就不会
 * 再变回最新（viewKey 只增不减），所以冻结是单向的
 */
const Frozen = memo(
  function Frozen({ children }: { frozen: boolean; children: ReactNode }) {
    return children
  },
  (_prev, next) => next.frozen,
)

interface ViewLayerProps {
  id: number
  state: LayerState
  /** 挂载时的视图类型（之后传入的值属于最新视图，忽略） */
  kind: PanelView
  /** 已被更新的视图替换（暂存期间仍可能显示着）：inert，视图内的键盘作用域停用 */
  retiring: boolean
  stageWidth: string
  transition: typeof presets.crossfade
  fadeSeconds: number
  onStaged: (id: number) => void
  onFaded: (id: number) => void
  onExited: (id: number) => void
  children: ReactNode
}

type Presence = NonNullable<ContextType<typeof PresenceContext>>

const noop = () => {}

/**
 * 给视图里 `m.*` 的 PresenceContext：暂存挂载时 `initial` 为 false（屏蔽进入动画，视图整体淡入），`release()`
 * （离开暂存）之后为 undefined，之后挂载的组件照常播放。Motion 只在组件创建时读取 `initial`，所以用 getter
 * 读内部状态、上下文对象始终不变——换对象会让视图里每个 m.* 在交换的那一帧重渲染
 */
function createPresence(id: string, staged: boolean): { value: Presence; release: () => void } {
  let blocked = staged
  return {
    value: {
      id,
      isPresent: true,
      register: () => noop,
      get initial() {
        return blocked ? false : undefined
      },
    },
    release: () => {
      blocked = false
    },
  }
}

function ViewLayer({
  id,
  state,
  kind,
  retiring,
  stageWidth,
  transition,
  fadeSeconds,
  onStaged,
  onFaded,
  onExited,
  children,
}: ViewLayerProps) {
  // 挂载时就在暂存层：从 opacity 0 起步，交换后淡入；会话的第一个视图直接显示
  const [mountedStaged] = useState(state === 'staged')
  const [viewKind] = useState(kind)
  const presenceId = useId()
  const [presence] = useState(() => createPresence(presenceId, mountedStaged))
  const isStaged = state === 'staged'
  useLayoutEffect(() => {
    if (!isStaged) presence.release()
  }, [isStaged, presence])
  // 显示状态（useViewActive）：在布局阶段翻转，订阅者（键盘作用域、自动聚焦、列表）在同一次提交里跟上
  const active = !isStaged && !retiring
  const [activity] = useState(() => createViewActivity(active))
  useLayoutEffect(() => {
    activity.set(active)
  }, [activity, active])

  // 暂存层提交之后分步准备（见文件头）：先在自己的任务里完成样式与布局；下一帧只剩绘制属性；再下一帧改为 opacity 0
  // 的合成层完成绘制；再下一帧开始时就绪。data-laid / data-primed 直接写 DOM（只是样式开关，不需要 React 重渲染）
  const layerRef = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    const el = layerRef.current
    if (!el) return
    if (!isStaged) {
      // 离开暂存：样式开关只对暂存层有意义，清掉（DOM 上的标记不由 React 管理）
      delete el.dataset.laid
      delete el.dataset.primed
      return
    }
    let frame = 0
    const timer = setTimeout(() => {
      // 解除跳过并读布局：在这个任务里完成暂存层的样式与布局（不在提交的任务里，也不和绘制属性挤在同一帧）
      el.dataset.laid = ''
      void el.offsetHeight
      frame = requestAnimationFrame(() => {
        frame = requestAnimationFrame(() => {
          el.dataset.primed = ''
          frame = requestAnimationFrame(() => onStaged(id))
        })
      })
    })
    return () => {
      clearTimeout(timer)
      cancelAnimationFrame(frame)
    }
  }, [isStaged, id, onStaged])

  const stateRef = useRef(state)
  useLayoutEffect(() => {
    stateRef.current = state
  }, [state])
  const fadedOut = useCallback(() => {
    if (stateRef.current === 'leaving') onFaded(id)
    else if (stateRef.current === 'exiting') onExited(id)
  }, [id, onFaded, onExited])
  // 兜底：淡出的完成回调因故没有到达时，最迟在时长 + exitSlack 之后照样继续
  const fading = state === 'leaving' || state === 'exiting'
  useEffect(() => {
    if (!fading) return
    const timer = setTimeout(fadedOut, ms(fadeSeconds + timing.exitSlack))
    return () => clearTimeout(timer)
  }, [fading, fadeSeconds, fadedOut])

  return (
    <m.div
      ref={layerRef}
      className="panel-view"
      data-view={viewKind}
      data-state={state}
      inert={isStaged || retiring}
      style={isStaged ? { width: stageWidth } : undefined}
      initial={mountedStaged ? { opacity: 0 } : false}
      animate={{ opacity: state === 'shown' ? 1 : 0 }}
      transition={transition}
      onAnimationComplete={(definition) => {
        // 只认淡出的完成（被打断的淡入不算）
        if ((definition as { opacity?: number }).opacity === 0) fadedOut()
      }}
    >
      <ViewActivityContext value={activity}>
        <PresenceContext value={presence.value}>{children}</PresenceContext>
      </ViewActivityContext>
    </m.div>
  )
}

function renderView(props: ViewSwitchRenderProps): ReactNode {
  return renderKind(props.payload.kind, props)
}

function renderKind<V extends PanelView>(view: V, props: ViewSwitchRenderProps): ReactNode {
  const { Component } = views[view]
  return <Component {...(props as unknown as ViewProps<V>)} />
}
