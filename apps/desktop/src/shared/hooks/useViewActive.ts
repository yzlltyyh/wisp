/**
 * 视图是否已显示。
 *
 * 面板的视图切换（panel/shell/ViewSwitch）先把新视图挂在**暂存层**里渲染：不可见、不参与布局、`inert`，按目标
 * 宽度排版，等旧视图淡出（或暂存好）后才放进布局并淡入——这样挂载与首次布局的开销不会落在切换中的那一帧里。
 * 视图「不在显示中」有两种：还在暂存层里；已被替换、正在淡出（同样 `inert`）。这期间：
 * - 视图里的元素拿不到焦点，所以「挂载即聚焦」必须等视图显示：用 `useAutoFocus`（TextField / TextArea /
 *   SearchField 的 `autoFocus` 已经走它），不要直接写 DOM 的 autofocus 或在挂载时调用 `focus()`；
 * - `useKeyScope` 注册的键盘作用域自动停用（新视图不提前抢键，旧视图不在淡出期间响应按键）。
 *
 * 不在 ViewSwitch 里（面板外壳本身、固定窗口、设置、展示页、测试）时恒为 true。
 */
import {
  createContext,
  useContext,
  useLayoutEffect,
  useRef,
  useSyncExternalStore,
  type RefObject,
} from 'react'

/**
 * 一个视图层的显示状态。用可订阅的小 store 而不是直接把布尔值放进上下文：状态翻转时只有订阅了它的组件
 * （键盘作用域、自动聚焦、列表）重渲染，React 不必在交换的那一帧遍历整棵视图子树去找上下文的使用者
 */
export interface ViewActivity {
  get(): boolean
  subscribe(listener: () => void): () => void
}

export interface ViewActivityController extends ViewActivity {
  set(active: boolean): void
}

const always: ViewActivity = { get: () => true, subscribe: () => () => {} }

/** 由 ViewSwitch 的每一层提供：显示中（含淡入）为 true；暂存中、被替换后淡出中为 false */
export const ViewActivityContext = createContext<ViewActivity>(always)

export function createViewActivity(initial: boolean): ViewActivityController {
  let active = initial
  const listeners = new Set<() => void>()
  return {
    get: () => active,
    subscribe: (listener) => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    set: (next) => {
      if (next === active) return
      active = next
      for (const l of listeners) l()
    },
  }
}

/** 所在视图是否在显示中（不在暂存层里，也没有被替换） */
export function useViewActive(): boolean {
  const activity = useContext(ViewActivityContext)
  return useSyncExternalStore(activity.subscribe, activity.get, activity.get)
}

/**
 * 挂载时自动聚焦（与 DOM 的 autofocus 语义相同：只看挂载时的意图，只聚焦一次）。所在视图还在暂存层时推迟到
 * 视图显示的那次提交里（布局阶段，绘制之前）。`preventScroll`：不让浏览器为了露出元素去滚动卡片或页面。
 */
export function useAutoFocus(
  ref: RefObject<HTMLElement | null>,
  autoFocus: boolean | undefined,
): void {
  const active = useViewActive()
  const pending = useRef(autoFocus === true)
  useLayoutEffect(() => {
    if (!pending.current || !active) return
    pending.current = false
    ref.current?.focus({ preventScroll: true })
  }, [active, ref])
}
