/**
 * 面板生命周期：
 *
 * - **prepare**：`flushSync` 提交新视图（卡片不可见）→ 同步读取布局高度（offsetHeight，不受进入动画的缩放影响）→ 立即 `panel_ready`；
 *   不依赖 rAF / ResizeObserver / Motion 回调 / setTimeout（`?hidden_raf=0` 的 mock 屏蔽了它们，40 ms 内照样 ready）。
 *   测量值不是有限非负数时退回视图的估计高度。invoke resolve（Rust 已在命令内 show）后才进入动画，
 *   下一帧报 `panel_painted`；
 * - **原位交叉淡入**：面板已显示时收到 `transition = crossfade` 的 prepare，卡片不动，只交叉淡入视图，
 *   同样同步测量并 `panel_ready`。新视图在 ViewSwitch 的暂存层里异步渲染，此时还不在布局里：ready 报的是当前
 *   （旧）视图的高度，旧内容淡出期间窗口不动；交换时外壳立即 `panel_resize` 新内容的高度；
 * - **退出**：Native 帧模式的退出是窗口 alpha（Rust 在 `panel_hide` 里淡出后 orderOut），所以**立即**调用
 *   `panel_hide`，fast 之后再卸载内容；窗口不能透明（Css + `shadow_margin == 0`）时同样立即 `panel_hide`
 *   （Rust 直接隐藏；不透明窗口里的透明度 / 缩放动画会露出 WebView 的默认底色）；其余 Css 帧模式先播放卡片的
 *   退出动画，播完（`completeExit`）再调用 `panel_hide`，兜底计时器 fast + exitSlack；
 * - **隐藏**：卸载视图、清空会话数据、`<html data-hidden>` 暂停 CSS 动画。
 *
 * **订阅时机**：面板入口在 `createRoot().render` **之前**调用 `installPanelEvents()`，
 * 三个面板事件的监听随页面一加载就注册（与 React 的第一次渲染、视图预热并行），不等外壳的 effect——冷页面（回收 /
 * 空闲卸载后重建）上等外壳的 effect 要晚 70–130 ms 才注册，Rust 的补发要等到那时才有人收。外壳挂载之前到达的 prepare 先存下
 * （只留最新的一个），外壳挂载（layout effect）后的下一个微任务里处理，仍是同步测量 + 立即 ready。
 *
 * **同步提交不依赖 store 订阅**：外壳读会话用 zustand 的 useSyncExternalStore，它的订阅在 passive
 * effect 里才建立——冷页面上挂载之前到达的 prepare 在 layout effect 之后的微任务里处理，这时订阅往往还没建立，
 * `flushSync(begin)` 改了 store 却没有任何组件重渲染，测到的是空卡片（退回估计高度：剪贴板 240 对真实的 451）。
 * 所以 prepare 的 `flushSync` 里同时让外壳自己重渲染一次（React 的 state，不经过订阅），重渲染时直接读到 store 的新
 * 会话，视图在同一次提交里进入布局。另外记下每个会话 ready 报的高度（`readyHeightOf`），高度跟随在窗口可见后的
 * 第一次测量与它不同时立即补一次 `panel_resize`（兜底：ready 万一报的是估计值，Css 帧的窗口也不会停在估计高度）。
 *
 * **冷唤出的 `Boot.prepare`**：空闲卸载 / 失效重建之后的唤出，Rust 把暂存的 prepare 随初始化
 * 脚本送进新页面。`installPanelEvents()` 先把它当作「外壳挂载之前到达的 prepare」存下（只用一次），外壳挂载后的微任务里
 * 照常同步提交、测量、`panel_ready`——不等页面加载完成、`listen` 的往返与 Rust 的补发。Rust 在页面加载后仍会 emit 同一
 * 会话作兜底，按会话去重。
 *
 * **Boot 里的会话可能已经结束**：页面 JS 运行之前 Rust 就可能结束了它——冷唤出的外壳上按了 Esc、输入 / 剪贴板
 * 视图失焦被收起、截图前隐藏取消了暂存——那时发出的 `panel:hide_request` 页面还没有监听，丢了。所以 `panel_ready` 回 null
 * （Rust 不认识这个会话：已结束、已被取代，或还没把暂存变成会话）时页面**复位为隐藏**：清空会话、卸载视图、恢复
 * `<html data-hidden>`（暂停骨架屏 / 光标等无限动画，排上资源释放计时器），不停在 preparing。Rust 之后真为这个会话 emit
 * prepare（还没把暂存变成会话的竞态）时阶段是 hidden，照常处理。初始化脚本在每次导航都会重跑：WebContent 崩溃后 Tauri 的
 * reload 会把建窗口时冻结的 `__WISP_BOOT__` 原样再注入一次，所以 `boot.prepare` 只在第一次导航采用（`reloaded()`）。
 */
import { useCallback, useLayoutEffect, useReducer } from 'react'
import { flushSync } from 'react-dom'
import { api, boot, onMany, type Unlisten } from '../../shared/ipc'
import type { HideReason, PanelPrepare } from '../../shared/ipc/types'
import { setWindowHidden } from '../../shared/hooks/useHidden'
import { duration, ms, timing } from '../../shared/motion/tokens'
import { viewOf } from '../views'
import { usePanelStore } from './panelStore'

/** Css 帧模式退出动画的兜底：动画回调因故没有到达时，最迟这么久后照样隐藏 */
const EXIT_FALLBACK_MS = ms(duration.fast + timing.exitSlack)

export interface PanelLifecycle {
  requestHide: (reason: HideReason) => void
  /** Css 帧模式：卡片退出动画播完 */
  completeExit: () => void
  /** 进入动画播完 */
  completeEnter: () => void
}

/** 同步测量；非法值退回视图的估计高度 */
function safeHeight(measured: number, prepare: PanelPrepare): number {
  const h =
    Number.isFinite(measured) && measured > 0 ? measured : viewOf(prepare.view).estimatedHeight
  return Math.min(Math.ceil(h), prepare.max_height)
}

/** Css 帧模式但窗口不能透明（Boot.shadow_margin 为 0）：卡片铺满窗口、纯色、直角 */
export function isOpaqueWindow(): boolean {
  return boot.frame_mode === 'css' && boot.shadow_margin === 0
}

/** 页面级订阅（`installPanelEvents`）；null = 还没有注册 */
let installed: Unlisten | null = null
/** 外壳挂载后处理 prepare 的入口；null = 外壳还没挂载 */
let sink: ((prepare: PanelPrepare) => void) | null = null
/** 外壳挂载之前到达的 prepare（只留最新的：Rust 每帧补发同一会话，新会话取代旧会话） */
let pending: PanelPrepare | null = null
/** 最近一次 ready 报给 Rust 的高度（窗口按它显示；高度跟随据此判断要不要立即补报，见文件头） */
let lastReady: { session: number; height: number } | null = null
/** 本页还没有发出过 ready：第一次发出时记一个时间点（每页一次，window lab 的冷唤出拆分用） */
let firstReady = true

/** 会话 ready 时报的内容高度；不是这个会话（或还没 ready）时为 null */
export function readyHeightOf(session: number): number | null {
  return lastReady?.session === session ? lastReady.height : null
}

/**
 * 注册面板事件（页面加载时调用一次，见文件头）；重复调用返回同一个取消函数。外壳挂载时如发现还没有注册（单测直接渲染
 * PanelApp），会自己注册、卸载时取消
 */
export function installPanelEvents(): Unlisten {
  if (installed) return installed
  // 冷唤出：随 Boot 送进来的 prepare（只用一次；之后的 prepare 都经事件到达）。重新加载出来的页面不用（见文件头）
  const early = boot.prepare
  boot.prepare = null
  if (early && !sink && !reloaded()) pending = early
  const un = onMany({
    'panel:prepare': (prepare) => {
      if (sink) sink(prepare)
      else pending = prepare
    },
    'panel:hide_request': ({ session_id, reason }) => {
      // 还没轮到处理的会话被要求隐藏：丢掉它（与「监听还没注册、事件丢失」的结果一致：Rust 按 ready 超时收尾）
      if (pending?.session_id === session_id) pending = null
      else if (usePanelStore.getState().prepare?.session_id === session_id) hide(reason)
    },
    'panel:focus_changed': ({ session_id, focused }) => {
      const s = usePanelStore.getState()
      if (s.prepare?.session_id === session_id) s.setFocused(focused)
    },
  })
  const off: Unlisten = () => {
    un()
    if (installed === off) installed = null
    pending = null
  }
  installed = off
  return off
}

/** 有一个 prepare 在等外壳挂载（冷唤出的 Boot.prepare，或挂载之前到达的事件）：入口据此同步完成第一次渲染 */
export function hasEarlyPrepare(): boolean {
  return pending !== null
}

/**
 * 本页是重新加载出来的（WebContent 崩溃后 Tauri 的 reload、`location.reload()`）：初始化脚本把建窗口时冻结的 Boot 原样
 * 再注入了一次，里面的 prepare 早已过时。读不到导航时序时按第一次导航处理（Rust 对过时的会话回 null，页面照样复位）
 */
function reloaded(): boolean {
  try {
    const nav = performance.getEntriesByType('navigation')[0] as
      PerformanceNavigationTiming | undefined
    return nav?.type === 'reload'
  } catch {
    return false
  }
}

/**
 * 处理一次 prepare（外壳已挂载：同步提交、同步测量、立即 ready）。`rerender` 让外壳在 flushSync 里重渲染（不依赖
 * store 订阅是否已经建立，见文件头）
 */
function onPrepare(prepare: PanelPrepare, measure: () => number, rerender: () => void): void {
  const s = usePanelStore.getState()
  const visible = s.phase === 'entering' || s.phase === 'shown'
  // 同一会话的重复 prepare（Rust 在新页面上、ready 超时之后补发；冷唤出时 Boot 与页面加载后的 emit）：本会话已在准备
  // 或显示中，ready 已发出——忽略，否则卡片会重新挂载、再播一次进入动画（Rust 的 panel_ready 对已显示的会话幂等）。
  // ready 被回了 null 的会话已复位为隐藏（见文件头），再来的 prepare 照常处理
  if (s.prepare?.session_id === prepare.session_id && (visible || s.phase === 'preparing')) return
  if (prepare.transition === 'crossfade' && visible && s.prepare) {
    flushSync(() => {
      s.crossfade(prepare)
      rerender()
    })
    // 新视图在暂存层里异步渲染、还不在布局里，测到的是旧视图——有意报旧高度，让窗口在旧内容淡出 / 新视图暂存
    // 期间保持不动；交换的那次提交之后由外壳立即 panel_resize（ViewSwitch.onSwapped → useHeightFollow.flush），
    // 宽度变化时 Rust 在 ready 时按新宽度安排交叉淡入
    const height = safeHeight(measure(), prepare)
    lastReady = { session: prepare.session_id, height }
    void api.panelReady(prepare.session_id, height).then((shown) => {
      const now = usePanelStore.getState()
      if (shown && now.prepare?.session_id === shown.session_id) now.setFocused(shown.focused)
    })
    return
  }
  setWindowHidden(false)
  flushSync(() => {
    s.begin(prepare)
    rerender()
  })
  const height = safeHeight(measure(), prepare)
  lastReady = { session: prepare.session_id, height }
  if (firstReady) {
    firstReady = false
    performance.mark('wisp:first-ready')
  }
  void api.panelReady(prepare.session_id, height).then((shown) => {
    const now = usePanelStore.getState()
    if (!shown) {
      // Rust 不认识这个会话（已结束、已被取代，或还没把暂存的 prepare 变成会话）：复位为隐藏（见文件头）。已被新会话
      // 取代（store 里已是别的会话）或已在退出流程里时不动
      if (now.prepare?.session_id === prepare.session_id && now.phase === 'preparing') {
        if (lastReady?.session === prepare.session_id) lastReady = null
        resetHidden()
      }
      return
    }
    if (now.prepare?.session_id !== shown.session_id) return
    now.setFocused(shown.focused)
    // ready 在路上时已经收到 hide_request（阶段已是 exiting）：不能把它改回 entering，否则退出流程
    // （Css 的 panel_hide、Native 的卸载与 data-hidden）会因阶段不对而被跳过
    if (now.phase !== 'preparing') return
    now.setPhase('entering')
    requestAnimationFrame(() => {
      void api.panelPainted(shown.session_id)
    })
  })
}

export function usePanelLifecycle(measure: () => number): PanelLifecycle {
  // 外壳自己的重渲染开关（见文件头「同步提交不依赖 store 订阅」）
  const [, rerender] = useReducer((n: number) => n + 1, 0)
  // layout effect：外壳第一次提交之后、绘制之前就接上（passive effect 在并发根上要等到绘制之后）
  useLayoutEffect(() => {
    const own = installed ? null : installPanelEvents()
    const handle = (prepare: PanelPrepare) => onPrepare(prepare, measure, rerender)
    sink = handle
    const early = pending
    pending = null
    // 挂载之前到达的 prepare：提交阶段里不能 flushSync，放到下一个微任务（仍在绘制之前）
    if (early)
      queueMicrotask(() => {
        if (sink === handle) handle(early)
      })
    return () => {
      if (sink === handle) sink = null
      own?.()
    }
  }, [measure])
  const completeEnter = useCallback(() => {
    const s = usePanelStore.getState()
    if (s.phase === 'entering') s.setPhase('shown')
  }, [])
  return { requestHide: hide, completeExit, completeEnter }
}

function hide(reason: HideReason): void {
  const s = usePanelStore.getState()
  const session = s.prepare?.session_id
  if (session === undefined || s.phase === 'exiting' || s.phase === 'hidden') return
  s.beginExit(reason)
  if (boot.frame_mode === 'native' || isOpaqueWindow()) {
    // Native：窗口 alpha 淡出由 Rust 承担；不透明窗口：Rust 直接隐藏，前端不播退出动画。
    // 前端都在 fast 之后再卸载内容（否则淡出中 / 还没隐藏的窗口里是空卡片）
    void api.panelHide(session, reason)
    setTimeout(() => finishExit(session), EXIT_FALLBACK_MS)
    return
  }
  setTimeout(() => completeExitOf(session), EXIT_FALLBACK_MS)
}

/** Css 帧模式：退出动画播完（或兜底计时器到期，先到者生效） */
function completeExit(): void {
  const session = usePanelStore.getState().prepare?.session_id
  if (session !== undefined) completeExitOf(session)
}

function completeExitOf(session: number): void {
  const s = usePanelStore.getState()
  if (s.prepare?.session_id !== session || s.phase !== 'exiting') return
  const reason = s.exitReason ?? 'other'
  void api.panelHide(session, reason)
  finishExit(session)
}

function finishExit(session: number): void {
  const s = usePanelStore.getState()
  if (s.prepare?.session_id !== session || s.phase !== 'exiting') return
  resetHidden()
}

/** 回到隐藏：卸载视图、清空会话数据、`<html data-hidden>`（暂停 CSS 动画、排上资源释放计时器） */
function resetHidden(): void {
  const s = usePanelStore.getState()
  s.setPhase('hidden')
  s.clear()
  setWindowHidden(true)
}
