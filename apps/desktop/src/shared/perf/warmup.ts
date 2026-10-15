/**
 * 空闲预热。面板窗口常驻、启动时就隐藏创建：趁空闲把「进程里第一次」才有的一次性开销
 * 先付掉，免得它落在第一次唤出 / 第一次切换视图的那一帧（或 prepare 的 40 ms 预算）里。
 *
 * - **字体**：第一次用到某个字体（字族 × 字重 × 字号，以及 CJK 回退字体）时，排版要实例化平台字体、查回退、冷启动
 *   整形缓存。实测（Chromium，`shell.view-switch` 里暂存剪贴板视图的首次布局）冷启动 5.7–6.8 ms，预热后 2.0–2.1 ms。
 *   这里按令牌（tokens.css）里视图会用到的组合各排一行中英混排样例：不可见、`contain: strict`、排完即移除；
 * - **Intl**：第一次构造 `Intl.NumberFormat` / `PluralRules` / `DateTimeFormat` 要加载 ICU 数据（实测约 5 ms）；
 *   这里调用 format.ts / relativeTime.ts 的公开函数，它们各自按 locale 缓存格式器。
 *
 * 每一步各占一个空闲任务（单步 ≤ 约 1–5 ms），不会连成长任务；WebKit 没有 requestIdleCallback，退回到计时器。
 * 界面语言切换后可以再调用一次（已缓存的步骤几乎是空操作）。
 */
import { formatBytes, formatDuration, formatTokens } from '../i18n/format'
import { relativeTime } from '../i18n/relativeTime'

/** 与 relativeTime 一致：超过 7 天显示日期 */
const DATE_AFTER_MS = 8 * 86_400_000

/** 字体预热的样例：CJK、拉丁字母、数字、全角与半角标点、URL */
const FONT_SAMPLE =
  '剪贴板与翻译 Clipboard & Translate 0123456789 「中文」，标点。https://example.com/a?b=1'

/** 视图会用到的字体组合（取自 tokens.css 的字号 / 字族令牌与组件样式里的字重、等宽数字） */
const FONT_STYLES: readonly string[] = [
  'font: 400 var(--text-body) var(--font-sans)',
  'font: 400 var(--text-ui) var(--font-sans)',
  'font: 500 var(--text-ui) var(--font-sans)',
  'font: 600 var(--text-ui) var(--font-sans)',
  'font: 600 var(--text-title) var(--font-sans)',
  'font: 400 var(--text-meta) var(--font-sans); font-variant-numeric: tabular-nums',
  'font: 600 var(--text-caption) var(--font-sans)',
  'font: 400 var(--text-ui) var(--font-mono)',
  'font: 400 var(--text-meta) var(--font-mono)',
]

/** 排一行样例并立即移除：不可见、不参与页面布局（contain: strict 的固定尺寸盒子） */
function warmFont(style: string): void {
  const el = document.createElement('div')
  el.setAttribute('aria-hidden', 'true')
  el.style.cssText = `position:fixed;left:0;top:0;width:640px;height:40px;contain:strict;visibility:hidden;pointer-events:none;white-space:nowrap;${style}`
  el.textContent = FONT_SAMPLE
  document.body.append(el)
  void el.offsetHeight
  el.remove()
}

const steps: ReadonlyArray<() => void> = [
  ...FONT_STYLES.map((style) => () => warmFont(style)),
  () => formatDuration(1500),
  () => formatTokens(2),
  () => {
    formatBytes(1536)
    formatBytes(15_360)
  },
  // 同年与跨年两种日期格式
  () => relativeTime(Date.now() - DATE_AFTER_MS),
  () => relativeTime(0),
]

/** 立即同步执行全部预热步骤（mock 场景模拟「常驻面板已在启动时预热过」的稳态时使用） */
export function warmUp(): void {
  for (const step of steps) step()
}

/** 空闲时逐步执行全部预热步骤，每步一个任务；返回取消函数（卸载时调用） */
export function warmUpWhenIdle(): () => void {
  let next = 0
  let cancel = () => {}
  const schedule = () => {
    if (next >= steps.length) return
    const run = () => {
      steps[next++]?.()
      schedule()
    }
    if (typeof requestIdleCallback === 'function') {
      const id = requestIdleCallback(run)
      cancel = () => cancelIdleCallback(id)
    } else {
      const timer = setTimeout(run, 0)
      cancel = () => clearTimeout(timer)
    }
  }
  schedule()
  return () => cancel()
}
