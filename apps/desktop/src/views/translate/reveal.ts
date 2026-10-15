/**
 * 长高 / 变矮时的「揭开」。
 *
 * 面板卡片的高度跟随内容是**先排版、后追赶**：视图按新的自然高度排好版，卡片（Css 帧模式：PanelCard 动画卡片高度）
 * 或窗口（Native 帧模式：Rust 逐帧改窗口高度）随后才追上来。追赶期间，排在内容最下面的底栏（状态行、[停止]、完成后的
 * 操作）会落在卡片的裁切线以外——看不见、点不到，正文也被拦腰切断。
 *
 * 这里让底栏始终贴住卡片当前**可见的**下沿：
 * - 长高：长高点 = 自上一次稳定以来，视图里最上面那个变高了的直接子元素（流式正文、展开的原文对照、新出现的提示条……）；
 *   它之后的兄弟元素 `translateY(−h)`，它自己 `clip-path` 裁掉底部 h；h = 内容自然下沿 − 可见下沿；
 * - 变矮：自上一次稳定以来在排版里**上移了**的直接子元素（收起的原文之后的正文与底栏、放开最小高度的正文之后的提示行与
 *   底栏……）各自 `translateY(+min(d, 上移量))`，随卡片下沿一起收上来；d = 可见下沿 − 内容自然下沿。已经脱离排版、
 *   正在退场的子元素（AnimatePresence 的 popLayout：收起中的原文）从下往上裁到紧随其后的元素的可见上沿——收起与展开
 *   对称，都只有卡片这一个弹簧在动。浮层预留（菜单撑高卡片）不算变矮；
 * - 只改 transform / clip-path，不影响排版——外壳测到的仍是自然高度，卡片照常追赶；追上之后去掉样式。
 *
 * 可见高度的来源：
 * - Css 帧模式：卡片元素当前的布局高度（Motion 逐帧写入的 height；减少动效时经 React 状态写入，晚 1–2 帧），
 *   ResizeObserver 每帧回调；
 * - Native 帧模式：窗口高度前端读不到，按与 Rust 相同的弹簧（spring.gentle，保留速度重定目标；tokens.ts 与
 *   `windows::sizing` 由单测对照）在前端同步推演；Rust 在下一帧才收到 `panel_resize`、再下一帧开始推进，推演相应
 *   晚两帧开始——宁可底栏比窗口下沿略高一点，也不被裁掉。减少动效时 Rust 直接改窗口高度：同样晚两帧，直接到位；
 * - 浏览器 mock 的 Native 帧模式：模拟窗口（#root）的高度由 CSS 过渡驱动，直接观察它。
 * 减少动效**不关闭**揭开（卡片 / 窗口的高度同样晚 1–2 帧，期间底栏会被裁掉、随每一行闪烁），只是没有过渡。
 * 窗口不能透明（opaque，卡片铺满窗口、高度立即生效）、Pinned（铺满窗口）时不需要。
 *
 * 菜单的下拉帘用的是卡片自己提供的可见下沿（PanelCard → 浮层边界的 `edge`，同一种推演口径），不经过这里。
 * 空闲时没有任何计时器：只有 ResizeObserver（尺寸变化时才回调）与追赶期间的弹簧。
 */
import { animate, motionValue, type AnimationPlaybackControls } from 'motion/react'
import { useEffect, useState, type RefObject } from 'react'
import { getTransport } from '../../shared/ipc'
import type { FrameMode } from '../../shared/ipc/types'
import { spring } from '../../shared/motion/tokens'
import { EdgeStore, type VisibleEdge } from '../../shared/ui/floating'

export type RevealSource = 'card' | 'spring' | 'root' | null

/** 可见高度从哪里来；null = 不需要揭开（卡片 / 窗口的高度与内容同步） */
export function revealSource(opts: {
  frame: FrameMode
  /** 窗口不能透明（shadow_margin = 0 的 Css 帧模式） */
  opaque: boolean
  pinned: boolean
}): RevealSource {
  if (opts.pinned) return null
  if (opts.frame === 'css') return opts.opaque ? null : 'card'
  if (__WISP_MOCK__ && getTransport().kind === 'mock') return 'root'
  return 'spring'
}

/** el 的上边相对 ancestor 上边的布局距离（不受 transform 影响） */
function offsetWithin(el: HTMLElement, ancestor: HTMLElement): number {
  let top = 0
  let node: HTMLElement | null = el
  while (node && node !== ancestor) {
    top += node.offsetTop
    const parent: Element | null = node.offsetParent
    if (!(parent instanceof HTMLElement)) break
    node = parent
  }
  return top
}

/** 上一次稳定时一个子元素的排版位置 */
interface Settled {
  readonly top: number
  readonly height: number
}

/**
 * 在 `root`（视图根元素，直接子元素是头部 / 原文 / 正文 / 提示 / 底栏）上启用揭开，返回卡片的可见高度。`active`：
 * 视图已显示（不在 prepare 的同步路径、不在暂存层里）
 */
export function useRevealEdge(
  root: RefObject<HTMLElement | null>,
  source: RevealSource,
  active: boolean,
  maxHeight: number,
  reduced: boolean,
): VisibleEdge {
  const [edge] = useState(() => new EdgeStore())
  useEffect(() => {
    const el = root.current
    if (!el || !source || !active) return
    const card = el.closest<HTMLElement>('.panel-card')
    const content = el.closest<HTMLElement>('.panel-content')
    if (!card || !content) return
    const padBottom = Number.parseFloat(getComputedStyle(content).paddingBottom) || 0
    const windowRoot = source === 'root' ? document.getElementById('root') : null

    /** 上一次稳定（h = 0）时各子元素的排版位置：判断长高点 / 上移的元素 */
    let settled = new Map<Element, Settled>()
    const snapshot = () => {
      settled = new Map()
      for (const child of el.children) {
        const c = child as HTMLElement
        settled.set(child, { top: c.offsetTop, height: c.offsetHeight })
      }
    }
    snapshot()
    let applied = 0
    let styled: HTMLElement[] = []
    const clear = () => {
      for (const s of styled) {
        s.style.removeProperty('transform')
        s.style.removeProperty('clip-path')
      }
      styled = []
    }
    const naturalBottom = () => offsetWithin(el, card) + el.offsetHeight + padBottom
    const outOfFlow = (c: HTMLElement) => getComputedStyle(c).position === 'absolute'

    const apply = (visible: number) => {
      const bottom = naturalBottom()
      let h = Math.round(bottom - visible)
      // 卡片比视图高是因为浮层预留了高度（内容区的最小高度），不是视图变矮：底栏留在原处
      if (h < 0 && content.offsetHeight > bottom + 1) h = 0
      edge.set(Math.round(visible))
      if (h === applied) return
      applied = h
      clear()
      if (h === 0) {
        snapshot()
        return
      }
      const children = Array.from(el.children) as HTMLElement[]
      if (h < 0) {
        // 变矮：在排版里上移了的元素留在原处、随卡片下沿收上来；正在退场（脱离排版）的元素裁到下一个元素的可见上沿
        const d = -h
        const shift = new Map<HTMLElement, number>()
        for (const c of children) {
          if (outOfFlow(c)) continue
          const was = settled.get(c)
          const moved = was ? was.top - c.offsetTop : 0
          if (moved > 0.5) {
            const y = Math.min(d, moved)
            shift.set(c, y)
            c.style.transform = `translateY(${y}px)`
            styled.push(c)
          }
        }
        // 没有任何元素上移（例如只是放开了正文区的最小高度）：正文之后的元素留在卡片下沿
        if (shift.size === 0) {
          const body = children.find((c) => c.classList.contains('tv-body')) ?? null
          for (
            let n = body?.nextElementSibling ?? children.at(-1) ?? null;
            n;
            n = n.nextElementSibling
          ) {
            if (!(n instanceof HTMLElement) || outOfFlow(n)) continue
            n.style.transform = `translateY(${d}px)`
            styled.push(n)
          }
        }
        for (let i = 0; i < children.length; i++) {
          const c = children[i] as HTMLElement
          if (!outOfFlow(c)) continue
          const next = children.slice(i + 1).find((n) => !outOfFlow(n))
          if (!next) continue
          const cut = c.offsetTop + c.offsetHeight - (next.offsetTop + (shift.get(next) ?? 0))
          if (cut <= 0) continue
          c.style.clipPath = `inset(0 0 ${Math.min(cut, c.offsetHeight)}px 0)`
          styled.push(c)
        }
        return
      }
      const grow =
        children.find((c) => !outOfFlow(c) && c.offsetHeight > (settled.get(c)?.height ?? 0)) ??
        children.at(-2) ??
        null
      if (!grow) return
      grow.style.clipPath = `inset(0 0 ${Math.min(h, grow.offsetHeight)}px 0)`
      styled.push(grow)
      for (let n = grow.nextElementSibling; n; n = n.nextElementSibling) {
        if (!(n instanceof HTMLElement) || outOfFlow(n)) continue
        n.style.transform = `translateY(${-h}px)`
        styled.push(n)
      }
    }

    // ---- 可见高度：卡片 / 模拟窗口（DOM 可读）
    if (source === 'card' || source === 'root') {
      const target = source === 'card' ? card : windowRoot
      if (!target) return
      const read = () => (source === 'card' ? card.offsetHeight : target.clientHeight)
      const observer = new ResizeObserver(() => apply(read()))
      observer.observe(el)
      observer.observe(target)
      apply(read())
      return () => {
        observer.disconnect()
        clear()
        edge.set(Infinity)
      }
    }

    // ---- 可见高度：与 Rust 同参的弹簧推演（Native 帧模式；减少动效时晚两帧直接到位）
    const targetHeight = () => Math.min(maxHeight, content.offsetHeight)
    const visible = motionValue(targetHeight())
    let playing: AnimationPlaybackControls | null = null
    let goal = visible.get()
    let frame = 0
    const unsubscribe = visible.on('change', (v) => apply(v))
    const retarget = () => {
      frame = 0
      playing?.stop()
      if (reduced) visible.set(goal)
      else playing = animate(visible, goal, spring.gentle)
    }
    const observer = new ResizeObserver(() => {
      const next = targetHeight()
      apply(visible.get())
      if (next === goal) return
      goal = next
      if (!frame) frame = requestAnimationFrame(() => (frame = requestAnimationFrame(retarget)))
    })
    observer.observe(el)
    // 浮层预留（菜单撑高内容区）同样会让窗口长高
    observer.observe(content)
    apply(visible.get())
    return () => {
      observer.disconnect()
      if (frame) cancelAnimationFrame(frame)
      playing?.stop()
      unsubscribe()
      clear()
      edge.set(Infinity)
    }
  }, [root, source, active, maxHeight, reduced, edge])
  return edge
}
