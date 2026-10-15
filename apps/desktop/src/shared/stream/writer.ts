/**
 * 流式文本写入器：plain 格式与 MdLite 共用的帧调度。
 *
 * - `append()` 只把文字放进缓冲，每个 rAF 合并处理一次（不逐字动画）；document 隐藏时改用 `setTimeout(50)` 兜底
 *   （隐藏的窗口不派发 rAF，流仍要写进去，显示时才是完整的）；
 * - plain：每帧最多追加一个 `<span class="chunk">`（两个 span 至少间隔 15 ms，保证同时动画的 ≤ 8 个），
 *   合并规则见 chunks.ts；全程只用 createElement / textContent，**禁止 innerHTML**；
 * - mdlite：每帧把缓冲交给 IncrementalMdLite，只重新解析尾块，交给 MdRenderer 做最小 DOM 更新；
 * - `end()`：冲掉缓冲、尾块按非乐观规则重解析、合并所有 span、光标淡出；
 * - URL 的折行点（urlBreaks.ts）：plain 在写进缓冲时由 UrlBreaker 增量插入，mdlite 由 render.ts 按行内 run 插入；
 *   `text` 返回的全文不含折行点；
 * - 每帧的工作量计入 FrameMonitor（开发者模式）。
 */
import { IncrementalMdLite } from '../markdown/incremental'
import { MdRenderer } from '../markdown/render'
import { prefersReducedMotion } from '../motion/useReducedMotionPref'
import { ms, duration } from '../motion/tokens'
import { measureWork } from '../perf/frameMonitor'
import { ChunkTracker } from './chunks'
import { UrlBreaker } from './urlBreaks'

/** 隐藏时的兜底节拍 */
const HIDDEN_TICK_MS = 50

export type WriterFormat = 'plain' | 'mdlite'

export interface WriterOptions {
  format?: WriterFormat
  /** 是否显示末尾光标（流式中） */
  caret?: boolean
  /** 每次把文字写进 DOM 之后（StreamView 用来跟随滚动） */
  onFlush?: () => void
}

export class StreamWriter {
  private buffer = ''
  private full = ''
  private scheduled: { kind: 'raf' | 'timeout'; id: number } | null = null
  private ended = false
  private readonly chunks: ChunkTracker
  private readonly format: WriterFormat
  private md: IncrementalMdLite | null = null
  private renderer: MdRenderer | null = null
  /** 流式中是否显示末尾光标 */
  private caret: boolean
  private caretTimer: ReturnType<typeof setTimeout> | null = null
  /** plain 格式的 URL 折行点（跨 append 保留状态：URL 可能被拆在两次 append 里） */
  private breaker = new UrlBreaker()

  constructor(
    private readonly container: HTMLElement,
    private readonly options: WriterOptions = {},
  ) {
    this.format = options.format ?? 'plain'
    this.caret = options.caret ?? true
    this.chunks = new ChunkTracker(() => !prefersReducedMotion())
    this.setupFormat()
  }

  /** 已写入（含缓冲中）的全文 */
  get text(): string {
    return this.full
  }

  get isEnded(): boolean {
    return this.ended
  }

  append(text: string): void {
    if (!text || this.ended) return
    // mdlite 的缓冲交给解析器（原文），折行点由 render.ts 按行内 run 插入（代码块不插）
    this.buffer += this.md ? text : this.breaker.feed(text)
    this.full += text
    this.schedule()
  }

  /** 立即把缓冲写进 DOM（prepare 路径、测试；不经 rAF） */
  flushNow(): void {
    this.cancelSchedule()
    this.flush(performance.now(), true)
  }

  /** 流结束：写完缓冲、合并全部 span、光标淡出 */
  end(): void {
    if (this.ended) return
    this.cancelSchedule()
    this.flush(performance.now(), true)
    this.ended = true
    measureWork(() => {
      if (this.md && this.renderer) {
        // 尾块按最终（非乐观）规则重解析：未闭合的 ** 原样显示
        this.renderer.update(this.md.takeFresh(), this.md.tail(false))
      }
      this.chunks.mergeAll()
    })
    this.setCaretState('ending')
    this.caretTimer = setTimeout(() => this.setCaretState(null), ms(duration.fast))
    this.options.onFlush?.()
  }

  /** 清空并（可选）写入一段初始文字（不做动画）；回到流式状态 */
  reset(text = ''): void {
    this.cancelSchedule()
    this.clearCaretTimer()
    this.chunks.forget()
    this.container.textContent = ''
    this.buffer = ''
    this.full = ''
    this.ended = false
    this.breaker = new UrlBreaker()
    this.setupFormat()
    if (text) {
      this.full = text
      this.writeStatic(text)
    }
    this.setCaretState(this.caret ? 'on' : null)
    this.options.onFlush?.()
  }

  /** 显示 / 隐藏末尾光标（例如首 token 前由外部 CursorPulse 负责时关掉） */
  setCaret(on: boolean): void {
    this.caret = on
    if (!this.ended) this.setCaretState(on ? 'on' : null)
  }

  destroy(): void {
    this.cancelSchedule()
    this.clearCaretTimer()
    this.chunks.dispose()
  }

  // ---------------------------------------------------------------------------

  private setupFormat(): void {
    if (this.format === 'mdlite') {
      this.md = new IncrementalMdLite()
      this.renderer = new MdRenderer(this.container, this.chunks)
    } else {
      this.md = null
      this.renderer = null
    }
    this.setCaretState(this.caret ? 'on' : null)
  }

  private writeStatic(text: string): void {
    if (this.md && this.renderer) {
      this.md.append(text)
      this.renderer.update(this.md.takeFresh(), this.md.tail(true), false)
    } else {
      this.container.append(document.createTextNode(this.breaker.feed(text)))
    }
  }

  private schedule(): void {
    if (this.scheduled) return
    if (typeof document !== 'undefined' && document.hidden) {
      const id = window.setTimeout(() => this.onTick(performance.now()), HIDDEN_TICK_MS)
      this.scheduled = { kind: 'timeout', id }
    } else {
      const id = requestAnimationFrame((now) => this.onTick(now))
      this.scheduled = { kind: 'raf', id }
    }
  }

  private cancelSchedule(): void {
    const s = this.scheduled
    if (!s) return
    if (s.kind === 'raf') cancelAnimationFrame(s.id)
    else clearTimeout(s.id)
    this.scheduled = null
  }

  private onTick(now: number): void {
    this.scheduled = null
    this.flush(now, false)
    // plain 模式下本帧没写（距上一个 span 太近）：下一帧再试
    if (this.buffer) this.schedule()
  }

  private flush(now: number, force: boolean): void {
    if (!this.buffer) return
    measureWork(() => {
      if (this.md && this.renderer) {
        this.md.append(this.buffer)
        this.buffer = ''
        this.renderer.update(this.md.takeFresh(), this.md.tail(true))
        this.setCaretState(this.caret ? 'on' : null)
        return
      }
      if (!force && !this.chunks.canAnimate(now) && !prefersReducedMotion()) return
      const node = this.chunks.create(this.buffer, now)
      this.buffer = ''
      this.container.append(node)
      // 减少动效时文本节点直接相连，顺手合并
      if (node.nodeType === Node.TEXT_NODE) this.container.normalize()
    })
    this.options.onFlush?.()
  }

  private setCaretState(state: 'on' | 'ending' | null): void {
    if (this.renderer) {
      this.renderer.setCaret(state)
      return
    }
    if (state === null) delete this.container.dataset.caret
    else this.container.dataset.caret = state === 'on' ? 'true' : 'ending'
  }

  private clearCaretTimer(): void {
    if (this.caretTimer !== null) clearTimeout(this.caretTimer)
    this.caretTimer = null
  }
}
