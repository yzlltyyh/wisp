/**
 * 高度跟随。窗口可见后用 ResizeObserver 观察内容高度（prepare 阶段不挂）：
 *
 * - **native**：每个 rAF 至多上报一次 `panel_resize(内容高度)`，窗口高度由 Rust 逐帧弹簧（spring.gentle，保留速度
 *   重定目标）驱动，卡片高度恒为 100%，前端不做高度动画；
 * - **css**：卡片高度在窗口内用 spring.gentle 跟随内容。变高：先上报（Rust 按 +48 阶梯放大窗口），卡片再长高；
 *   变矮：卡片先播完收缩动画，再上报最终高度，窗口一次缩到位（不会出现窗口先缩、卡片被裁切的帧）；
 * - **opaque**（Css 帧模式但窗口不能透明，`shadow_margin == 0`，例如 Wayland）：卡片铺满窗口、没有高度动画可等，
 *   与 native 一样每个 rAF 至多上报一次，变高变矮都立即上报（Rust 直接把窗口设为卡片高度）。
 *
 * 窗口可见后的第一次测量与 ready 报的高度（`readyHeightOf`）不同时（ready 之后内容又变了，或 ready 报的是估计值）
 * 立即补报：native / opaque 本来就在第一帧上报；css 如果把第一次测量当成「已上报」，窗口会停在 ready 的高度、卡片
 * 被裁掉。
 *
 * **native 的可见下沿**（`edge`，交给卡片的浮层边界做菜单的下拉帘）：窗口高度前端读不到，按与 Rust 相同的弹簧
 * （spring.gentle，保留速度重定目标；tokens.ts 与 `windows::sizing` 由单测对照）推演——每次上报之后晚一帧开始（Rust
 * 下一帧收到、再下一帧推进，与 translate/reveal.ts 同一口径）；没有订阅者（没有菜单打开）时直接到位、不推演。
 *
 * **浮层预留用 snappy**：菜单等浮层预留内容高度引起的长高、以及释放预留之后紧接着的那次回收，
 * native 上报时带 `spring = snappy`（Rust 的窗口弹簧随之换参数，位置与速度连续），可见下沿的推演同样用 spring.snappy——
 * 与 Css 帧卡片高度的选择是同一条规则（PanelCard 维护 `ResizeHint`，`quickResize` 判定）。
 */
import { animate, motionValue, type AnimationPlaybackControls } from 'motion/react'
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type RefObject } from 'react'
import { api } from '../../shared/ipc'
import { duration, ms, spring } from '../../shared/motion/tokens'
import { EdgeStore, type VisibleEdge } from '../../shared/ui/floating'
import { readyHeightOf } from './usePanelLifecycle'

/** 高度跟随方式：Native 帧模式、Css 帧模式（透明窗口里的卡片）、Css 帧模式但窗口不能透明 */
export type HeightFollowMode = 'native' | 'css' | 'opaque'

/**
 * 浮层预留的状态（外壳持有的 ref：PanelCard 在提交阶段经 `noteReserve` 写、高度跟随在下一帧读，不经过 React 状态）：
 * 当前预留的内容最小高度，与释放预留之后「回收窗口」的截止时刻（performance.now）
 */
export interface ResizeHint {
  reserved: number
  releaseUntil: number
}

/** 释放预留之后多久之内的高度变化算「回收」（用 spring.snappy）：内容的 ResizeObserver → 下一帧上报通常只要 1–2 帧 */
export const RELEASE_WINDOW_MS = ms(duration.slow)

/** 记下预留的变化（从有到无时开始回收窗口） */
export function noteReserve(hint: ResizeHint, reserved: number): void {
  if (hint.reserved > 0 && reserved === 0) hint.releaseUntil = performance.now() + RELEASE_WINDOW_MS
  hint.reserved = reserved
}

/**
 * 这次高度变化是不是浮层预留引起的（用 spring.snappy）：有预留且内容至少到预留高度（卡片目标就是预留本身，或预留期间的
 * 变化），或刚释放预留、还在回收窗口里。Css 帧卡片高度与 Native 帧窗口弹簧共用这一条
 */
export function quickResize(reserved: number, height: number | null, releasing: boolean): boolean {
  return (reserved > 0 && height !== null && height >= reserved - 1) || releasing
}

export interface HeightFollow {
  /** css：卡片目标高度（null = prepare 阶段，跟随自然高度）；native / opaque 恒为 null */
  cardHeight: number | null
  /** css：卡片高度动画结束时由 PanelCard 调用（其余方式没有卡片高度动画，为空操作） */
  onHeightSettled: () => void
  /**
   * 立即测量并上报（不等 ResizeObserver + rAF）。视图切换在新视图进入布局、新宽度生效的那次提交之后调用，
   * Rust 在同一帧开始高度过渡；面板不可见时为空操作
   */
  flush: () => void
  /** native：窗口可见下沿的推演（见文件头）；其余方式不使用 */
  edge: VisibleEdge
}

export function useHeightFollow(
  ref: RefObject<HTMLElement | null>,
  sessionId: number | null,
  active: boolean,
  mode: HeightFollowMode,
  /** 卡片上限（PanelPrepare.max_height）：内容再高也只报到上限，超出部分由视图内部滚动 */
  maxHeight: number = Infinity,
  /** 减少动效：native 的推演晚两帧直接到位（Rust 同样直接改窗口高度） */
  reduced = false,
  /** 浮层预留的状态（外壳持有、PanelCard 维护）：native 据此给 `panel_resize` 指定 snappy（见文件头） */
  hint?: RefObject<ResizeHint>,
): HeightFollow {
  const [edge] = useState(() => new EdgeStore())
  // 按会话记录：新会话（prepare 阶段）不会沿用上一次的卡片高度
  const [card, setCard] = useState<{ session: number; height: number } | null>(null)
  /** Css：已上报给 Rust 的卡片高度、等待收缩动画结束后再上报的高度、当前动画目标 */
  const reported = useRef(0)
  const pendingShrink = useRef<number | null>(null)
  const target = useRef<number | null>(null)
  const flushRef = useRef<(() => void) | null>(null)
  const sessionRef = useRef(sessionId)
  useLayoutEffect(() => {
    sessionRef.current = sessionId
  }, [sessionId])

  useEffect(() => {
    const el = ref.current
    if (!el || sessionId === null || !active) return
    // 布局高度（不受进入动画的 scale 影响；getBoundingClientRect 会把 0.96 的缩放算进去）
    const measure = () => Math.min(maxHeight, el.offsetHeight)
    if (mode === 'css') {
      // 卡片从此刻的内容高度起跳；窗口按 ready 时报的高度显示——两者不同时立即补报（见文件头）
      const h = measure()
      const sent = readyHeightOf(sessionId) ?? h
      reported.current = h
      if (h > 0 && h !== sent) void api.panelResize(sessionId, h)
      pendingShrink.current = null
      target.current = h
      setCard({ session: sessionId, height: h })
    }
    // native：窗口可见下沿的推演（窗口此刻按 ready / 上一次的高度显示）
    const mirror = motionValue(measure())
    let playing: AnimationPlaybackControls | null = null
    let mirrorFrame = 0
    const offMirror = mirror.on('change', (v) => edge.set(v))
    if (mode === 'native') edge.set(mirror.get())
    const follow = (h: number, quick: boolean) => {
      if (mirrorFrame) cancelAnimationFrame(mirrorFrame)
      mirrorFrame = requestAnimationFrame(() => {
        mirrorFrame = 0
        playing?.stop()
        if (reduced || !edge.watched) mirror.jump(h)
        else playing = animate(mirror, h, quick ? spring.snappy : spring.gentle)
      })
    }
    let frameId = 0
    let last = -1
    const onFrame = () => {
      frameId = 0
      const h = measure()
      if (h === last) return
      last = h
      if (mode === 'native') {
        // Rust 逐帧弹簧；浮层预留引起的变化指定 snappy（见文件头）
        const r = hint?.current
        const quick = !!r && quickResize(r.reserved, h, performance.now() < r.releaseUntil)
        void api.panelResize(sessionId, h, quick ? 'snappy' : undefined)
        follow(h, quick)
        return
      }
      if (mode === 'opaque') {
        // 卡片铺满窗口，没有卡片动画可等，变矮也立即上报
        void api.panelResize(sessionId, h)
        return
      }
      if (h > reported.current) {
        reported.current = h
        void api.panelResize(sessionId, h)
      }
      pendingShrink.current = h < reported.current ? h : null
      target.current = h
      setCard({ session: sessionId, height: h })
    }
    const observer = new ResizeObserver(() => {
      if (!frameId) frameId = requestAnimationFrame(onFrame)
    })
    observer.observe(el)
    flushRef.current = () => {
      if (frameId) cancelAnimationFrame(frameId)
      onFrame()
    }
    return () => {
      flushRef.current = null
      observer.disconnect()
      if (frameId) cancelAnimationFrame(frameId)
      if (mirrorFrame) cancelAnimationFrame(mirrorFrame)
      playing?.stop()
      offMirror()
      edge.set(Infinity)
    }
  }, [ref, sessionId, active, mode, maxHeight, reduced, edge, hint])

  const flush = useCallback(() => flushRef.current?.(), [])

  const onHeightSettled = useCallback(() => {
    const shrink = pendingShrink.current
    const session = sessionRef.current
    if (shrink === null || session === null || shrink !== target.current) return
    pendingShrink.current = null
    reported.current = shrink
    void api.panelResize(session, shrink)
  }, [])

  const cardHeight =
    mode === 'css' && card !== null && card.session === sessionId ? card.height : null
  return { cardHeight, onHeightSettled, flush, edge }
}
