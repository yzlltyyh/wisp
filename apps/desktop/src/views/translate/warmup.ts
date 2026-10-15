/**
 * 翻译视图的字体预热。
 *
 * Markdown-lite 正文（technical / learn / 词典式）会用到正文字号的**粗体、斜体**与行内代码字号的等宽字体——这几个组合
 * 不在外壳的空闲预热（shared/perf/warmup.ts）里。进程里第一次排版某个字体组合要实例化平台字体、查回退、冷启动整形缓存，
 * 实测（Chromium，`translate.stress-stream` 切到 technical 的头两帧）带来 5–9 ms 的整页布局；预热之后消失。
 * 以后可以并入外壳的启动预热（shared/perf/warmup.ts），之后删掉本文件。
 *
 * 两条路径，都只做一次、每步一个空闲任务（WebKit 没有 requestIdleCallback，用较长的间隔代替 setTimeout(0)）：
 * - **后台**（主路径）：模块加载后（面板窗口启动时隐藏创建）等一会儿，在**窗口隐藏**且没有视图在流式输出时逐步预热——
 *   第一次唤出之前就已做完，进程启动后第一次出现 Markdown 正文时不再有那次整页布局。窗口显示出来（或有视图在流式）就
 *   停下，再次隐藏时（`<html data-hidden>` 变化，MutationObserver，空闲时没有任何计时器）从断点继续；
 * - **兜底**：视图显示、没有流式输出、再等一会儿之后（`scheduleMarkdownWarmup`，视图隐藏 / 又开始流式时取消），覆盖
 *   后台还没轮到就被唤出、以及固定窗口（没有隐藏阶段）。
 * `translate.stress-stream` 场景在挂载前同步调用 `warmMarkdownFonts()`，对应的正是「后台预热已经做完」的稳态。
 */
import { isHidden } from '../../shared/hooks/useHidden'

/** 样例：CJK、拉丁字母、数字、标点 */
const SAMPLE = '流式渲染 Streaming render 0123456789 ≤ ≈ % → · 「标点」，。'

const STYLES: readonly string[] = [
  'font: 600 var(--text-body) var(--font-sans)',
  'font: italic 400 var(--text-body) var(--font-sans)',
  'font: italic 600 var(--text-body) var(--font-sans)',
  'font: 400 calc(var(--text-body) * 0.9) var(--font-mono)',
  'font: 600 var(--text-meta) var(--font-sans)',
  'font: 400 var(--text-body) var(--font-sans)',
  'font: 600 var(--text-title) var(--font-sans)',
  'font: 400 var(--text-meta) var(--font-mono)',
]

/** 空闲多久之后开始（ms，不是动效时长：避开进入动画、底栏淡入与紧随其后的用户操作；后台路径避开启动） */
const START_DELAY_MS = 1000
/** 每步之间的间隔（ms；有 requestIdleCallback 时间隔之后再等一个空闲时段） */
const STEP_GAP_MS = 150

function warm(style: string): void {
  const el = document.createElement('div')
  el.setAttribute('aria-hidden', 'true')
  el.style.cssText = `position:fixed;left:0;top:0;width:640px;height:40px;contain:strict;visibility:hidden;pointer-events:none;white-space:nowrap;${style}`
  el.textContent = SAMPLE
  document.body.append(el)
  void el.offsetHeight
  el.remove()
}

/** 下一个要预热的组合 */
let next = 0

/** 立即同步预热（mock 场景模拟「常驻面板已经预热过」的稳态） */
export function warmMarkdownFonts(): void {
  while (next < STYLES.length) warm(STYLES[next++] as string)
}

/** 是否已经全部预热 */
export function markdownFontsWarm(): boolean {
  return next >= STYLES.length
}

/**
 * 空闲时逐步预热，返回取消函数（视图隐藏、又开始流式时调用；之后再次调用从断点继续）
 */
export function scheduleMarkdownWarmup(): () => void {
  if (typeof document === 'undefined' || markdownFontsWarm()) return () => undefined
  let timer: ReturnType<typeof setTimeout> | null = null
  let idle: number | null = null
  let cancelled = false
  const step = () => {
    idle = null
    timer = null
    if (cancelled) return
    const style = STYLES[next]
    if (style === undefined) return
    next++
    warm(style)
    schedule(STEP_GAP_MS)
  }
  const schedule = (delay: number) => {
    if (cancelled || markdownFontsWarm()) return
    timer = setTimeout(() => {
      timer = null
      if (cancelled) return
      if (typeof requestIdleCallback === 'function') idle = requestIdleCallback(step)
      else step()
    }, delay)
  }
  schedule(START_DELAY_MS)
  return () => {
    cancelled = true
    if (timer !== null) clearTimeout(timer)
    if (idle !== null) cancelIdleCallback(idle)
  }
}

// -----------------------------------------------------------------------------
// 后台预热（见模块注释）

/** 正在流式输出的视图数：大于 0 时后台预热暂停 */
let holds = 0

/** 视图流式输出期间暂停后台预热；返回释放函数 */
export function holdMarkdownWarmup(): () => void {
  holds++
  let released = false
  return () => {
    if (released) return
    released = true
    holds--
    if (holds === 0) resumeBackground()
  }
}

let backgroundTimer: ReturnType<typeof setTimeout> | null = null
let backgroundIdle: number | null = null
let hiddenObserver: MutationObserver | null = null

const backgroundReady = () => holds === 0 && isHidden()

function backgroundStep(): void {
  backgroundTimer = null
  backgroundIdle = null
  if (markdownFontsWarm()) {
    hiddenObserver?.disconnect()
    hiddenObserver = null
    return
  }
  // 窗口显示中 / 有视图在流式：停下，等再次隐藏（MutationObserver）或释放（holdMarkdownWarmup）
  if (!backgroundReady()) return
  const style = STYLES[next]
  if (style !== undefined) {
    next++
    warm(style)
  }
  scheduleBackground(STEP_GAP_MS)
}

function scheduleBackground(delay: number): void {
  if (backgroundTimer !== null || backgroundIdle !== null || markdownFontsWarm()) return
  backgroundTimer = setTimeout(() => {
    backgroundTimer = null
    if (typeof requestIdleCallback === 'function')
      backgroundIdle = requestIdleCallback(backgroundStep)
    else backgroundStep()
  }, delay)
}

function resumeBackground(): void {
  if (backgroundReady()) scheduleBackground(STEP_GAP_MS)
}

/** 面板窗口：模块加载后开始后台预热（只在有 `<html data-hidden>` 隐藏阶段的面板窗口里有效） */
function startBackground(): void {
  if (typeof document === 'undefined' || typeof MutationObserver === 'undefined') return
  hiddenObserver = new MutationObserver(resumeBackground)
  hiddenObserver.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ['data-hidden'],
  })
  scheduleBackground(START_DELAY_MS)
}

startBackground()
