/**
 * 流式文字块的淡入与合并（Writer 与 MdLite 渲染器共用）。
 *
 * - 新到达的文字放进 `<span class="chunk">`（CSS 动画 animate-chunk-in：opacity 0→1，120 ms）；
 * - 任何时刻仍在动画中的 span 不超过 8 个（超过时新文字直接以文本节点出现）；
 * - 定期合并：动画已播完（≥ 120 ms）的 span 换成文本节点并 `normalize()` 并入前面的文本，
 *   触发条件为累计满 32 个 span（在创建第 32 个之前合并）或空闲 500 ms；`mergeAll()`（流结束）全部合并；
 * - 减少动效时不建 span，直接追加文本节点（DOM 更少，视觉上就是立即出现）。
 * 只使用 createElement / createTextNode / textContent，禁止 innerHTML。
 */
import { ms, timing } from '../motion/tokens'

/** 同时在动画中的 span 上限 */
export const MAX_ANIMATING = 8
/** 累计到这么多个 span 就合并（创建第 32 个之前） */
export const MERGE_AT = 32
export const CHUNK_MS = ms(timing.chunkIn)
/** 两个动画 span 之间的最小间隔：保证同时动画的不超过 MAX_ANIMATING 个 */
export const MIN_SPAN_INTERVAL_MS = CHUNK_MS / MAX_ANIMATING

interface LiveSpan {
  el: HTMLElement
  at: number
}

export class ChunkTracker {
  private spans: LiveSpan[] = []
  private idle: ReturnType<typeof setTimeout> | null = null
  private lastAt = -Infinity

  constructor(private readonly animate: () => boolean) {}

  /** 当前存活的 span 数（测试 / 性能观测） */
  get count(): number {
    return this.spans.length
  }

  animatingCount(now: number): number {
    let n = 0
    for (let i = this.spans.length - 1; i >= 0; i--) {
      const s = this.spans[i]
      if (!s || now - s.at >= CHUNK_MS) break
      n++
    }
    return n
  }

  /** 距离上一个 span 是否已经足够久（Writer 据此决定本帧建 span 还是继续缓冲） */
  canAnimate(now: number): boolean {
    return now - this.lastAt >= MIN_SPAN_INTERVAL_MS
  }

  /** 为一段新文字创建节点：动画中的 span，或（减少动效 / 超过上限时）文本节点 */
  create(text: string, now: number = performance.now()): Node {
    if (!this.animate() || this.animatingCount(now) >= MAX_ANIMATING) {
      return document.createTextNode(text)
    }
    if (this.spans.length >= MERGE_AT - 1) this.mergeFinished(now)
    const el = document.createElement('span')
    el.className = 'chunk'
    el.textContent = text
    this.spans.push({ el, at: now })
    this.lastAt = now
    this.scheduleIdle()
    return el
  }

  /** 合并动画已播完的 span */
  mergeFinished(now: number = performance.now()): void {
    let k = 0
    const parents = new Set<Node>()
    while (k < this.spans.length) {
      const s = this.spans[k] as LiveSpan
      if (now - s.at < CHUNK_MS) break
      this.unwrap(s.el, parents)
      k++
    }
    if (k > 0) this.spans.splice(0, k)
    for (const p of parents) p.normalize()
  }

  /** 全部合并（流结束、重置） */
  mergeAll(): void {
    const parents = new Set<Node>()
    for (const s of this.spans) this.unwrap(s.el, parents)
    this.spans = []
    for (const p of parents) p.normalize()
    this.clearIdle()
  }

  /** 丢弃跟踪（容器被清空时） */
  forget(): void {
    this.spans = []
    this.clearIdle()
  }

  dispose(): void {
    this.forget()
  }

  private unwrap(el: HTMLElement, parents: Set<Node>): void {
    const parent = el.parentNode
    if (!parent) return
    el.replaceWith(document.createTextNode(el.textContent ?? ''))
    parents.add(parent)
  }

  private scheduleIdle(): void {
    this.clearIdle()
    this.idle = setTimeout(() => {
      this.idle = null
      const now = performance.now()
      this.mergeFinished(now)
      // 仍有在动画中的：等它们播完再合并一次
      if (this.spans.length > 0) this.scheduleIdle()
    }, ms(timing.writerIdleMerge))
  }

  private clearIdle(): void {
    if (this.idle !== null) clearTimeout(this.idle)
    this.idle = null
  }
}
