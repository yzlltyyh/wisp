/**
 * 帧监视。只在开发者模式或 mock 的 `?perf=1` 下运行，面板隐藏时停止。
 *
 * WKWebView 与 WebKitGTK 都不支持 `longtask`，流畅度一律用 rAF 采样衡量：
 * - 帧间隔直方图：≤ 8.3 / ≤ 16.7 / ≤ 33 / > 33 ms；
 * - 掉帧：帧间隔 > 1.5 × 实测帧周期（统计窗口内帧间隔的中位数，按实际刷新率取；与 qa:shots 的 DROP_FACTOR 同一口径）。
 *   rAF 本身有 ±1 ms 的抖动（60 Hz 下在 16.6–17.2 ms 之间），按「> 16.7 ms」判定会把一半的空闲帧记成掉帧；
 *   采样不足时按 60 Hz 估计（> 25 ms）；
 * - 是否告警：p95 > max(17 ms, 帧周期 × 1.15)（qa:shots 的 FRAME_JITTER）或每帧 JS > 8 ms；
 * - 每帧 JS 耗时：Writer / MdLite / 列表渲染用 `measureWork` 包住自己的工作量，按帧累计；
 * - 统计窗口为最近 240 帧（约 2–4 s），另有本次运行的累计直方图。
 * 结果给 FrameMonitorView 显示，并挂在 `window.__WISP_PERF__` 上供自动化读取。
 */
import { boot } from '../ipc/boot'

export interface FrameStats {
  /** 统计窗口内的帧数 */
  frames: number
  p50: number
  p95: number
  max: number
  /** 累计直方图 */
  buckets: { le8: number; le16: number; le33: number; gt33: number }
  /** 累计掉帧数（帧间隔 > 1.5 × 帧周期） */
  dropped: number
  /** 实测帧周期（统计窗口内帧间隔的中位数，ms） */
  period: number
  /** 实测刷新率（Hz，由帧周期换算） */
  hz: number
  /** 窗口内每帧 JS 耗时 */
  jsAvg: number
  jsP95: number
  jsMax: number
}

const WINDOW = 240
const FRAME_60 = 1000 / 60
const FRAME_120 = 1000 / 120
const FRAME_30 = 1000 / 30
/** 超过帧周期这么多倍的帧记为掉帧（与 scripts/qa/screenshots.ts 的 DROP_FACTOR 一致） */
export const DROP_FACTOR = 1.5
/** p95 相对帧周期的抖动容差（与 scripts/qa/screenshots.ts 的 FRAME_JITTER 一致） */
export const FRAME_JITTER = 1.15
/** p95 的告警下限（60 Hz 一帧） */
const P95_BUDGET = 17
/** 每帧 JS 的告警阈值 */
const JS_BUDGET = 8
/** 至少这么多帧之后才用实测帧周期（之前按 60 Hz 估计） */
const MIN_PERIOD_SAMPLES = 30

let consumers = 0
let workThisFrame = 0
let raf = 0
let last = 0
const intervals: number[] = []
const work: number[] = []
const buckets = { le8: 0, le16: 0, le33: 0, gt33: 0 }
let dropped = 0
/** 当前的帧周期估计（每秒按窗口中位数更新一次） */
let period = FRAME_60
const listeners = new Set<(s: FrameStats) => void>()
let reportTimer: ReturnType<typeof setInterval> | null = null

declare global {
  interface Window {
    __WISP_PERF__?: { snapshot(): FrameStats; reset(): void }
  }
}

/** 是否应该启用（开发者模式，或 mock 下的 `?perf=1`） */
export function frameMonitorEnabled(): boolean {
  if (boot.dev) return true
  if (!__WISP_MOCK__ || typeof location === 'undefined') return false
  return new URLSearchParams(location.search).get('perf') === '1'
}

/** 是否正在采样（Writer 等据此决定要不要计时，未启用时零开销） */
export function isMonitoring(): boolean {
  return consumers > 0
}

/** 记一笔本帧的 JS 工作量（毫秒） */
export function recordWork(ms: number): void {
  if (consumers > 0) workThisFrame += ms
}

/** 计时执行一段工作（未启用时直接执行） */
export function measureWork<T>(fn: () => T): T {
  if (consumers === 0) return fn()
  const t0 = performance.now()
  try {
    return fn()
  } finally {
    workThisFrame += performance.now() - t0
  }
}

function percentile(sorted: readonly number[], p: number): number {
  if (sorted.length === 0) return 0
  const i = Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1))
  return sorted[i] as number
}

/** 一份统计是否该告警（HUD 标红） */
export function framesWarn(stats: FrameStats): boolean {
  return stats.p95 > Math.max(P95_BUDGET, stats.period * FRAME_JITTER) || stats.jsMax > JS_BUDGET
}

export function snapshot(): FrameStats {
  const s = [...intervals].sort((a, b) => a - b)
  const w = [...work].sort((a, b) => a - b)
  const jsSum = work.reduce((a, b) => a + b, 0)
  // 帧周期：窗口内帧间隔的中位数（按实际刷新率；ProMotion 屏上 WKWebView 的 rAF 也可能跑在 60 Hz）
  if (s.length >= MIN_PERIOD_SAMPLES) period = percentile(s, 0.5)
  return {
    frames: intervals.length,
    p50: round(percentile(s, 0.5)),
    p95: round(percentile(s, 0.95)),
    max: round(s[s.length - 1] ?? 0),
    buckets: { ...buckets },
    dropped,
    period: round(period),
    hz: period > 0 ? Math.round(1000 / period) : 0,
    jsAvg: round(work.length ? jsSum / work.length : 0),
    jsP95: round(percentile(w, 0.95)),
    jsMax: round(w[w.length - 1] ?? 0),
  }
}

function round(n: number): number {
  return Math.round(n * 10) / 10
}

export function resetFrameStats(): void {
  intervals.length = 0
  work.length = 0
  buckets.le8 = buckets.le16 = buckets.le33 = buckets.gt33 = 0
  dropped = 0
  period = FRAME_60
}

function tick(now: number): void {
  raf = requestAnimationFrame(tick)
  if (last > 0) {
    const dt = now - last
    intervals.push(dt)
    work.push(workThisFrame)
    if (intervals.length > WINDOW) intervals.shift()
    if (work.length > WINDOW) work.shift()
    if (dt <= FRAME_120 + 0.5) buckets.le8++
    else if (dt <= FRAME_60 + 0.5) buckets.le16++
    else if (dt <= FRAME_30 + 0.5) buckets.le33++
    else buckets.gt33++
    if (dt > period * DROP_FACTOR) dropped++
  }
  workThisFrame = 0
  last = now
}

/** 开始采样（引用计数）；`onStats` 每秒回调一次。返回停止函数 */
export function startFrameMonitor(onStats?: (stats: FrameStats) => void): () => void {
  consumers++
  if (onStats) listeners.add(onStats)
  if (consumers === 1) {
    last = 0
    period = FRAME_60
    raf = requestAnimationFrame(tick)
    reportTimer = setInterval(() => {
      const s = snapshot()
      for (const l of listeners) l(s)
    }, 1000)
    if (typeof window !== 'undefined') {
      window.__WISP_PERF__ = { snapshot, reset: resetFrameStats }
    }
  }
  let stopped = false
  return () => {
    if (stopped) return
    stopped = true
    if (onStats) listeners.delete(onStats)
    consumers--
    if (consumers === 0) {
      cancelAnimationFrame(raf)
      raf = 0
      if (reportTimer !== null) clearInterval(reportTimer)
      reportTimer = null
    }
  }
}
