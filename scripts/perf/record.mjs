#!/usr/bin/env node
/**
 * scripts/perf/record.mjs：把 measure-*.sh / measure-windows.ps1 采集的原始样本汇总成性能记录
 * （`wisp.perf/1`，格式见 README），按 schema 校验后写到 target/perf/<date>-<os>.json（或 --out 指定的路径）。
 *
 * 平台脚本只负责采样（各平台的进程与计数器接口不同），统计与格式集中在这里，三端口径一致。
 *
 * 用法：
 *   node scripts/perf/record.mjs --raw <样本文件> [--out <文件>] [--latency-log <日志>] [--app-log <wisp.log>]
 *                                [--smoke <smoke-report.json>]…
 *     --out 省略时写 target/perf/<YYYY-MM-DD>-<os>.json（已存在则加 -2、-3…）；`--out -` 只打印不写文件
 *     --smoke 可重复，每种构建类型（release / debug）最多一份，分别存进 `smoke.release` / `smoke.debug`
 *     --app-log 被测应用这次运行的日志（measure-*.sh 启动应用时自动给出）：
 *       - 样本文件带 exercise 时刻（meta exercise_start_ms / exercise_end_ms）时得出 `idle.session`：exercise 之后日志里的
 *         perf 追踪数与面板显示过的次数（exercise 是否真的唤出了面板）、面板回收 / 空闲卸载的时刻，以及采样窗口的状态
 *         （hot 回收前 / recycled 回收后 / unloaded 卸载后 / mixed 窗口内发生了）——smoke-gate 的 --expect-state 据此判定；
 *       - 没有 --latency-log 时，日志里本次启动（meta launch_ms 之后）的 perf 追踪同时作为 latency 段（没有追踪时不写）；
 *       - macOS：本次启动的预检行 `preflight … launch_env=true|false` 写成 `idle.launch_env`（Info.plist 的 LSEnvironment——
 *         `MallocSpaceEfficient=1`——是否生效；读不到时为 null），smoke-gate 对经 LaunchServices 启动的记录核对
 *   node scripts/perf/record.mjs --update <记录> --smoke <smoke-report.json>… [--out <文件>]
 *     只替换已有记录（例如基线 scripts/perf/baselines/macos.json）里对应构建类型的冒烟段，空闲 / 延迟段原样保留；
 *     默认写回原文件。用来给只有空闲段的基线补上 `smoke.release` / `smoke.debug`，或在冒烟口径变化后单独刷新
 *   node scripts/perf/record.mjs --self-test   用构造的样本检查统计口径（CI 用）
 *
 * 样本文件（UTF-8 文本，每行一条，空格分隔；`#` 开头为注释）：
 *   meta <键> <值…>   os / os_version / arch / machine / build / version / webview_attribution / wakeups_method /
 *                     wakeups_start / wakeups_end / wakeups_per_s（主进程）/ wv_wakeups_start / wv_wakeups_end /
 *                     wv_wakeups_per_s（WebView 进程）/ launch_ms（脚本启动应用的时刻）/ exercise_start_ms /
 *                     exercise_end_ms / settle_s（--exercise 的时刻与静置秒数）/ note（可多条）。wv_wakeups_start / end
 *                     的值为 `pid:累计次数,…`；时刻为 Unix 毫秒；任何数值为 `-` 表示取不到
 *   sample <epoch_ms> <主进程 RSS KiB> <主进程累计 CPU 毫秒> <WebView>
 *     <WebView> 为 `pid:RSS KiB:累计 CPU 毫秒,…`（这一刻属于 Wisp 的全部 WebView 进程），取不到时为 `-`
 */
import assert from 'node:assert/strict'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { parseArgs } from 'node:util'
import { APP_DIR, ROOT, isMain, ok, readJson, rel, run } from '../lib/common.mjs'
import { latencyFromFiles } from './latency-report.mjs'
import {
  PERF_SCHEMA,
  PanelTracker,
  latencyFromMarks,
  latencyFromSmoke,
  logLineMs,
  logSince,
  mean,
  parseLatencyLog,
  parsePanelEvents,
  round,
  smokeBuild,
  smokeSummary,
  validateDocument,
} from './lib.mjs'

/** `pid:a:b,…` → Map<pid, number[]>；`-` 或空为 null */
function parsePidList(text, width, where) {
  if (text === undefined || text === '-' || text === '') return null
  const out = new Map()
  for (const entry of text.split(',')) {
    const parts = entry.split(':').map(Number)
    if (parts.length !== width + 1 || !parts.every((v) => Number.isFinite(v) && v >= 0)) {
      throw new Error(`${where}：「${entry}」应为 pid${':<数值>'.repeat(width)}`)
    }
    out.set(parts[0], parts.slice(1))
  }
  return out
}

/** 解析样本文件 */
export function parseRaw(text) {
  const meta = { notes: [] }
  const samples = []
  for (const [i, raw] of text.split(/\r?\n/).entries()) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) continue
    const parts = line.split(/\s+/)
    const where = `第 ${i + 1} 行`
    if (parts[0] === 'meta') {
      const key = parts[1]
      const value = parts.slice(2).join(' ')
      if (!key) throw new Error(`${where}：meta 缺少键`)
      if (key === 'note') meta.notes.push(value)
      else meta[key] = value
    } else if (parts[0] === 'sample') {
      const [t, rss, cpu, wv, ...extra] = parts.slice(1)
      const s = { t: Number(t), rss: Number(rss), cpu: Number(cpu) }
      if (extra.length || ![s.t, s.rss, s.cpu].every((v) => Number.isFinite(v))) {
        throw new Error(
          `${where}：样本格式应为 sample <ms> <RSS KiB> <CPU ms> <pid:RSS KiB:CPU ms,…|->`,
        )
      }
      s.wv = parsePidList(wv, 2, where)
      samples.push(s)
    } else {
      throw new Error(`${where}：无法识别的记录 "${parts[0]}"`)
    }
  }
  return { meta, samples: samples.toSorted((a, b) => a.t - b.t) }
}

/** meta 里的数值；不存在或为 `-` 时为 null */
function metaNumber(meta, key) {
  const v = meta[key]
  if (v === undefined || v === '-' || v === '') return null
  const n = Number(v)
  if (!Number.isFinite(n) || n < 0) throw new Error(`meta ${key} 不是有效的数值：${v}`)
  return n
}

/** 两个 `pid → [累计值]` 之间、两边都有的进程的增量之和；没有共同进程时为 null */
function sumDelta(before, after) {
  if (!before || !after) return null
  let total = 0
  let common = 0
  for (const [pid, [v0]] of before) {
    const v1 = after.get(pid)?.[0]
    if (v1 === undefined) continue
    common++
    total += Math.max(0, v1 - v0)
  }
  return common ? total : null
}

/** 样本 → idle 段 */
export function idleFromSamples(meta, samples) {
  if (samples.length < 2) throw new Error(`样本不足（${samples.length} 个），至少需要 2 个`)
  const first = samples[0]
  const last = samples.at(-1)
  const spanMs = last.t - first.t
  if (spanMs <= 0) throw new Error('样本时间跨度为 0')
  const notes = [...meta.notes]
  const mib = (kib) => kib / 1024

  // 主进程 CPU：累计 CPU 时间之差 ÷ 墙钟时间（不受逐秒 %cpu 的采样时刻与平滑算法影响）
  let cpuMax = 0
  // WebView CPU：相邻两个样本里都存在的 WebView 进程的累计 CPU 之差（进程增减不会让总和跳变）
  let wvCpu = 0
  let wvCovered = 0
  let wvCpuMax = 0
  for (let i = 1; i < samples.length; i++) {
    const a = samples[i - 1]
    const b = samples[i]
    const dt = b.t - a.t
    if (dt <= 0) continue
    cpuMax = Math.max(cpuMax, ((b.cpu - a.cpu) / dt) * 100)
    const cpuOf = (m) => m && new Map([...m].map(([pid, [, cpu]]) => [pid, [cpu]]))
    const d = sumDelta(cpuOf(a.wv), cpuOf(b.wv))
    if (d === null) continue
    wvCpu += d
    wvCovered += dt
    wvCpuMax = Math.max(wvCpuMax, (d / dt) * 100)
  }
  const cpuAvg = Math.max(0, ((last.cpu - first.cpu) / spanMs) * 100)
  const cpuWebview = wvCovered > 0 ? (wvCpu / wvCovered) * 100 : null
  if (cpuWebview !== null && wvCovered < spanMs * 0.5) {
    notes.push(
      `WebView 进程只在 ${Math.round((wvCovered / spanMs) * 100)}% 的采样时间里有连续数据，CPU 按这段时间计算`,
    )
  }

  const withWv = samples.filter((s) => s.wv)
  const wvRss = withWv.map((s) => [...s.wv.values()].reduce((sum, [r]) => sum + r, 0))

  // 唤醒：主进程与 WebView 分别计算，口径见 wakeups_method
  const spanS = spanMs / 1000
  let wakeups = metaNumber(meta, 'wakeups_per_s')
  if (wakeups === null) {
    const start = metaNumber(meta, 'wakeups_start')
    const end = metaNumber(meta, 'wakeups_end')
    if (start !== null && end !== null) wakeups = Math.max(0, end - start) / spanS
  }
  let wakeupsWebview = metaNumber(meta, 'wv_wakeups_per_s')
  if (wakeupsWebview === null) {
    const d = sumDelta(
      parsePidList(meta.wv_wakeups_start, 1, 'meta wv_wakeups_start'),
      parsePidList(meta.wv_wakeups_end, 1, 'meta wv_wakeups_end'),
    )
    if (d !== null) wakeupsWebview = d / spanS
  }
  const method = meta.wakeups_method ?? null
  const attribution = meta.webview_attribution ?? null
  if (
    attribution !== null &&
    !['responsible', 'responsible-approx', 'descendants'].includes(attribution)
  ) {
    throw new Error(`meta webview_attribution 无效：${attribution}`)
  }

  const memoryMetric = meta.memory_metric ?? 'rss'
  if (!['rss', 'phys_footprint', 'working_set'].includes(memoryMetric)) {
    throw new Error(`meta memory_metric 无效：${memoryMetric}`)
  }

  return {
    duration_s: round(spanS, 1),
    samples: samples.length,
    // rss_* 字段的口径：macOS phys_footprint，Windows / Linux 沿用 rss
    memory_metric: memoryMetric,
    rss_main: round(mib(mean(samples.map((s) => s.rss))), 1),
    rss_main_max: round(mib(Math.max(...samples.map((s) => s.rss))), 1),
    rss_webview: wvRss.length ? round(mib(mean(wvRss)), 1) : null,
    rss_webview_max: wvRss.length ? round(mib(Math.max(...wvRss)), 1) : null,
    webview_processes: withWv.length ? Math.max(...withWv.map((s) => s.wv.size)) : null,
    webview_attribution: withWv.length ? attribution : null,
    cpu_avg: round(cpuAvg, 3),
    cpu_max: round(Math.max(0, cpuMax), 2),
    cpu_webview_avg: cpuWebview === null ? null : round(cpuWebview, 3),
    cpu_webview_max: cpuWebview === null ? null : round(wvCpuMax, 2),
    // 空闲 CPU 预算（< 0.1%）按主进程 + WebView 合计判定
    cpu_total_avg: cpuWebview === null ? null : round(cpuAvg + cpuWebview, 3),
    wakeups: wakeups === null ? null : round(wakeups, 2),
    wakeups_webview: wakeupsWebview === null ? null : round(wakeupsWebview, 2),
    wakeups_method: wakeups === null && wakeupsWebview === null ? null : method,
    notes,
  }
}

// ---------------------------------------------------------------------------
// 应用日志（时间戳与面板事件的解析在 lib.mjs，与 exercise.mjs 共用）

export { logLineMs, logSince }

/** windows::panel 的回收 / 空闲卸载日志（`on_idle_timer`） */
const RECYCLE_LINE = /\bpanel idle \d+ ms after use, recycling webview\b/
const UNLOAD_LINE = /\bpanel idle, unloading webview\b/

/**
 * 会话后空闲的核对信息（`idle.session`）：样本文件没有 exercise 时刻时为 null。时刻都换算成 exercise 结束后的秒数；
 * `logText` 为 null 表示读不到日志（shown / traces 为 null，state 与 panel 为 unknown）。
 *
 * 面板在采样窗口里是否隐藏着（`panel`）、exercise 唤出过哪些视图（`views`）、隐藏原因（`hides`）、最后一次开始隐藏的
 * 时刻（`last_hide_s`）与剪贴板记录（`clipboard`：新条目按类型计数，置顶的已有条目记为 `touched`）来自 debug 级别的日志（lib.mjs 的 parsePanelEvents）；日志里没有
 * debug 行时这些都是 null / unknown——不能把「没有事件」当作「面板已隐藏」。`log_lines` 是采样窗口里写出的日志行数
 * （空闲时应接近 0；debug 级别本身不该给空闲带来活动）
 */
export function sessionFromLog(meta, samples, logText, logName = null) {
  const exStart = metaNumber(meta, 'exercise_start_ms')
  const exEnd = metaNumber(meta, 'exercise_end_ms')
  if (exStart === null || exEnd === null || samples.length === 0) return null
  const sec = (ms) => round((ms - exEnd) / 1000, 1)
  const window = [sec(samples[0].t), sec(samples.at(-1).t)]
  const session = {
    settle_s: metaNumber(meta, 'settle_s') ?? Math.max(0, window[0]),
    window_s: window,
    traces: null,
    shown: null,
    recycles_s: [],
    unloads_s: [],
    state: 'unknown',
    panel: 'unknown',
    views: null,
    hides: null,
    last_hide_s: null,
    clipboard: null,
    log_lines: null,
    log: logName,
  }
  if (logText === null || logText === undefined) return session
  const after = logSince(logText, exStart)
  const { marks } = parseLatencyLog(after)
  session.traces = marks.length
  session.shown = marks.filter((m) => Number.isFinite(m.shown_ms)).length
  const [w0, w1] = window
  const [w0ms, w1ms] = [samples[0].t, samples.at(-1).t]
  let logLines = 0
  for (const line of after.split('\n')) {
    const t = logLineMs(line)
    if (t === null) continue
    if (t >= w0ms && t <= w1ms) logLines++
    if (RECYCLE_LINE.test(line)) session.recycles_s.push(sec(t))
    else if (UNLOAD_LINE.test(line)) session.unloads_s.push(sec(t))
  }
  session.log_lines = logLines
  const events = [
    ...session.recycles_s.map((t) => [t, 'recycled']),
    ...session.unloads_s.map((t) => [t, 'unloaded']),
  ].toSorted((a, b) => a[0] - b[0])
  if (events.some(([t]) => t >= w0 && t <= w1)) session.state = 'mixed'
  else {
    const before = events.filter(([t]) => t < w0)
    session.state = before.length ? before.at(-1)[1] : 'hot'
  }
  // 面板事件（debug 级别）：采样窗口开始时有面板在屏幕上、或窗口里有面板上屏 → visible
  const panel = parsePanelEvents(after)
  if (panel.debug) {
    const tracker = new PanelTracker()
    for (const e of panel.events) tracker.add(e)
    const shown = tracker.shown()
    const shownInWindow = shown.some((s) => s.shown >= w0ms && s.shown <= w1ms)
    session.panel = tracker.visibleAt(w0ms).length > 0 || shownInWindow ? 'visible' : 'hidden'
    session.views = shown.map((s) => s.view ?? 'unknown')
    session.hides = tracker.hideReasons()
    const ends = shown.map((s) => s.ended).filter((t) => t !== null)
    session.last_hide_s = ends.length ? sec(Math.max(...ends)) : null
    session.clipboard = { ...tracker.clips }
  }
  return session
}

/** macOS 剪贴板轮询的用户空闲分档与屏幕空闲闸门（wisp-platform `macos::poll_gate`，debug 级别） */
const TIER_LINE = /\bclipboard polling tier (Active|Quiet|Away)\b/
const SCREENS_LINE = /\bclipboard polling (slowed: screens idle|resumed)\b/
/** 记进历史的一次复制：新条目（inserted）或置顶已有条目（touched，内容与历史里的相同）——与 session.clipboard 同一口径 */
const COPY_LINE = /\bclipboard item (?:inserted|touched) id=\d+/
/** exercise 之后轮询仍处于活跃档的正当时长：最后一次剪贴板变化 / 面板 nudge 之后保持 30 s，留 2 s 余量 */
export const EXERCISE_ACTIVE_S = 32

/**
 * 采样窗口里的用户活动（`idle.activity`）：开发机上有人在用时（输入不断、偶尔复制），macOS 的剪贴板轮询
 * 一直在 4 Hz 的活跃档，空闲 CPU / 唤醒自然高于「用户不在」时录的基线——记下窗口里各档的时长与复制次数，smoke-gate
 * 据此把这类窗口的 CPU / 唤醒判定降为告警，而不是误判成退化。
 *
 * - `tiers_s`：窗口里各档的秒数（active / quiet / away，屏幕空闲闸门打开的时段计入 screens_idle）；只有 macOS 有分档日志，
 *   其他平台为 null。应用启动时是活跃档（`Tier::default`），之后按日志换档；
 * - `copies`：窗口里记进历史的复制次数（任何平台；新条目与置顶的已有条目都算——重复复制已有内容也是有人在用）；
 * - `user_active_s`：不能用 exercise 解释的活跃档时长——有 exercise 时扣掉 exercise 结束后 EXERCISE_ACTIVE_S 秒以内的部分；
 * - 日志里没有 debug 行时整个字段为 null（无从判断，不当作「没人在用」）
 */
export function activityFromLog(meta, samples, logText) {
  if (logText === null || logText === undefined || samples.length < 2) return null
  const launch = metaNumber(meta, 'launch_ms')
  const text = launch === null ? logText : logSince(logText, launch - 1000)
  const [w0, w1] = [samples[0].t, samples.at(-1).t]
  const exEnd = metaNumber(meta, 'exercise_end_ms')
  const explainedUntil = exEnd === null ? -Infinity : exEnd + EXERCISE_ACTIVE_S * 1000
  let debug = false
  let tierSeen = false
  let tier = 'active'
  let screensIdle = false
  let copies = 0
  const spans = { active: 0, quiet: 0, away: 0, screens_idle: 0 }
  let userActive = 0
  let cursor = null
  // 从 cursor 到 t（都截到窗口内）的这段时间按当前档位记账
  const advance = (t) => {
    const from = Math.max(cursor ?? w0, w0)
    const to = Math.min(t, w1)
    if (to > from) {
      const key = screensIdle ? 'screens_idle' : tier
      spans[key] += to - from
      if (key === 'active') userActive += Math.max(0, to - Math.max(from, explainedUntil))
    }
    cursor = t
  }
  for (const line of text.split(/\r?\n/)) {
    const t = logLineMs(line)
    if (t === null) continue
    if (!debug && /^\S+ DEBUG /.test(line)) debug = true
    const tm = TIER_LINE.exec(line)
    if (tm) {
      advance(t)
      tierSeen = true
      tier = tm[1].toLowerCase()
      continue
    }
    const sm = SCREENS_LINE.exec(line)
    if (sm) {
      advance(t)
      screensIdle = sm[1] !== 'resumed'
      continue
    }
    if (COPY_LINE.test(line) && t >= w0 && t <= w1) copies++
  }
  if (!debug) return null
  advance(w1)
  const secs = (ms) => round(ms / 1000, 1)
  // macOS 才有分档；其他平台没有分档日志（事件驱动，没有轮询）
  const tiers = meta.os === 'macos' || tierSeen
  return {
    tiers_s: tiers
      ? {
          active: secs(spans.active),
          quiet: secs(spans.quiet),
          away: secs(spans.away),
          screens_idle: secs(spans.screens_idle),
        }
      : null,
    copies,
    user_active_s: tiers ? secs(userActive) : null,
  }
}

/** idle.activity → notes 里的一句说明 */
export function activityNote(a) {
  if (!a) return null
  const tiers = a.tiers_s
    ? `剪贴板轮询 活跃 ${a.tiers_s.active} s / 安静 ${a.tiers_s.quiet} s / 离开 ${a.tiers_s.away} s` +
      (a.tiers_s.screens_idle ? ` / 屏幕空闲 ${a.tiers_s.screens_idle} s` : '')
    : null
  const busy = a.copies > 0 || (a.user_active_s ?? 0) > 0
  return `采样窗口里的用户活动：${[tiers, `复制 ${a.copies} 次`].filter(Boolean).join('，')}${busy ? '——有人在用，CPU / 唤醒高于「用户不在」时的基线是正常的（smoke-gate 只告警），需要在安静的机器上重测' : ''}`
}

/**
 * 本次运行（launch_ms 之后）的更新检查（`updater`：`update check trigger=…`）：一次性的网络请求与 TLS 初始化，落进采样窗口
 * 会抬高 CPU / 唤醒。返回 notes 里的一句说明；没有 launch_ms（--pid 测量已在运行的进程）时为 null
 */
export function updateCheckNote(logText, launchMs, samples) {
  if (launchMs === null || samples.length === 0) return null
  const checks = []
  for (const line of logSince(logText, launchMs - 1000).split('\n')) {
    if (/\bupdate check trigger=/.test(line)) checks.push(logLineMs(line))
  }
  if (checks.length === 0)
    return '更新检查：本次运行没有发生（测量设置关闭了更新检查，或还没到启动后 30 s）'
  const [w0, w1] = [samples[0].t, samples.at(-1).t]
  const inWindow = checks.filter((t) => t >= w0 && t <= w1).length
  return `更新检查：本次运行发生 ${checks.length} 次（启动后 ${checks.map((t) => round((t - launchMs) / 1000, 1)).join('、')} s），${inWindow ? `其中 ${inWindow} 次落在采样窗口里（这一窗的 CPU / 唤醒会偏高）` : '都不在采样窗口里'}`
}

/** 启动日志的预检行（app::bootstrap）：`preflight … launch_env=true|false`（只有 macOS 打印 launch_env） */
const LAUNCH_ENV_LINE = /\bpreflight\b.*\blaunch_env=(true|false)\b/

/**
 * 这次启动（launch_ms 之后的第一条预检行）的启动环境是否生效（`idle.launch_env`）：经 LaunchServices /
 * LaunchAgent 启动时 Info.plist 的 `LSEnvironment`（`MallocSpaceEfficient=1`）应当生效。只有 macOS 有这一项；读不到日志、
 * 不是 macOS 或日志里没有这一项（旧版本）时为 null。没有 launch_ms（--pid 测量已在运行的进程）时取日志里最后一次启动的
 */
export function launchEnvFromLog(meta, logText) {
  if (meta.os !== 'macos' || logText === null || logText === undefined) return null
  const launch = metaNumber(meta, 'launch_ms')
  const text = launch === null ? logText : logSince(logText, launch - 1000)
  let found = null
  for (const line of text.split(/\r?\n/)) {
    const m = LAUNCH_ENV_LINE.exec(line)
    if (!m) continue
    found = m[1] === 'true'
    if (launch !== null) break
  }
  return found
}

/** 读应用日志；不存在 / 读不了时为 null */
function readLog(path) {
  try {
    return readFileSync(path, 'utf8')
  } catch {
    return null
  }
}

/** 读取并校验冒烟报告，按构建类型分组；同一构建类型出现两份时报错 */
function loadSmokes(paths) {
  const byBuild = {}
  const reports = []
  for (const path of paths) {
    const report = readJson(path)
    const { kind, errors } = validateDocument(report)
    if (kind !== 'smoke' || errors.length) {
      throw new Error(`${path} 不是合法的冒烟报告：\n  ${errors.join('\n  ')}`)
    }
    if (!report.ok) {
      throw new Error(
        `${path} 的冒烟没有通过（ok=false：${report.failures.join('；')}），不能并入性能记录`,
      )
    }
    const build = smokeBuild(report)
    if (byBuild[build]) throw new Error(`--smoke 给了两份 ${build} 构建的冒烟报告`)
    byBuild[build] = smokeSummary(report)
    reports.push(report)
  }
  return { byBuild, reports }
}

/** 汇总成完整的性能记录（不写文件） */
export function buildRecord({ rawText, latencyLog, appLog, smokePaths = [], now = new Date() }) {
  const { meta, samples } = parseRaw(rawText)
  if (!['macos', 'windows', 'linux'].includes(meta.os)) {
    throw new Error(`样本文件缺少 meta os（macos / windows / linux），实际为 ${meta.os}`)
  }
  const smokes = loadSmokes(smokePaths)
  for (const r of smokes.reports) {
    if (r.os !== meta.os) throw new Error(`冒烟报告是 ${r.os} 的，样本是 ${meta.os} 的`)
  }
  const appLogText = appLog ? readLog(appLog) : null
  let latency = null
  if (latencyLog) {
    latency = latencyFromFiles([latencyLog]).latency
    if (latency.count === 0)
      throw new Error(`${latencyLog} 中没有面板显示过的 perf 追踪（需要开发者模式）`)
  } else if (appLogText !== null) {
    // 本次启动之后的追踪（--home keep 时日志里还有以前的运行）；一条都没有时不写 latency 段
    const launch = metaNumber(meta, 'launch_ms')
    const text = launch === null ? appLogText : logSince(appLogText, launch - 1000)
    const fromLog = latencyFromMarks(parseLatencyLog(text).marks)
    if (fromLog.count > 0 || fromLog.overlay_count > 0) latency = fromLog
  } else if (smokes.reports.length) {
    // 多份冒烟报告时，延迟取 release 的（与预算同口径）
    const r = smokes.reports.find((x) => smokeBuild(x) === 'release') ?? smokes.reports[0]
    latency = latencyFromSmoke(r)
  }
  const record = {
    schema: PERF_SCHEMA,
    ts: now.toISOString(),
    os: meta.os,
    os_version: meta.os_version ?? null,
    arch: meta.arch ?? null,
    version: meta.version || readJson(join(APP_DIR, 'package.json')).version,
    build: ['release', 'debug'].includes(meta.build) ? meta.build : null,
    machine: meta.machine ?? null,
    idle: samples.length ? idleFromSamples(meta, samples) : null,
    latency,
    smoke: smokes.reports.length ? smokes.byBuild : null,
  }
  if (record.idle && appLogText !== null) {
    const note = updateCheckNote(appLogText, metaNumber(meta, 'launch_ms'), samples)
    if (note) record.idle.notes.push(note)
  }
  if (record.idle && meta.os === 'macos') {
    // 启动环境（MallocSpaceEfficient=1）是否生效：smoke-gate 对经 LaunchServices 启动的记录核对
    record.idle.launch_env = launchEnvFromLog(meta, appLogText)
  }
  if (record.idle && appLogText !== null) {
    const activity = activityFromLog(meta, samples, appLogText)
    if (activity) {
      record.idle.activity = activity
      record.idle.notes.push(activityNote(activity))
    }
  }
  if (record.idle) {
    const session = sessionFromLog(
      meta,
      samples,
      appLogText,
      appLog ? appLog.split(/[\\/]/).at(-1) : null,
    )
    if (session) {
      record.idle.session = session
      if (session.shown === null) {
        record.idle.notes.push(
          `读不到应用日志${appLog ? `（${appLog}）` : '（没有 --app-log）'}：无法核对 exercise 是否唤出了面板、采样窗口是回收前还是回收后`,
        )
      } else if (session.panel === 'unknown') {
        record.idle.notes.push(
          '应用日志里没有 debug 级别的行（advanced.log_level 不是 debug）：无法核对采样窗口里面板是否已隐藏、exercise 唤出了哪些视图（measure-*.sh 在隔离 HOME 里会预置 debug）',
        )
      }
    }
  }
  const { errors } = validateDocument(record)
  if (errors.length)
    throw new Error(`生成的记录不符合 schema（脚本缺陷）：\n  ${errors.join('\n  ')}`)
  return record
}

/**
 * 把冒烟报告并入已有的性能记录：只替换报告对应构建类型的 `smoke.<build>`，其余字段（含另一种构建类型的冒烟段）原样
 * 保留，并在 notes 里记一笔。返回新记录（不写文件）
 */
export function updateRecord(record, smokePaths, now = new Date()) {
  const { kind, errors } = validateDocument(record)
  if (kind !== 'perf' || errors.length) {
    throw new Error(`要更新的不是合法的性能记录（wisp.perf/1）：\n  ${errors.join('\n  ')}`)
  }
  if (smokePaths.length === 0) throw new Error('--update 需要至少一份 --smoke')
  const smokes = loadSmokes(smokePaths)
  for (const r of smokes.reports) {
    if (r.os !== record.os) throw new Error(`冒烟报告是 ${r.os} 的，记录是 ${record.os} 的`)
  }
  const builds = Object.keys(smokes.byBuild)
  const next = {
    ...record,
    smoke: { ...record.smoke, ...smokes.byBuild },
    notes: [
      ...(record.notes ?? []),
      `${now.toISOString().slice(0, 10)}：record.mjs --update 并入 ${builds.join(' / ')} 冒烟（${smokes.reports
        .map((r) =>
          `${smokeBuild(r)} ${r.os_version ?? ''} ${r.arch ?? ''}`.replace(/\s+/g, ' ').trim(),
        )
        .join('；')}）`,
    ],
  }
  const check = validateDocument(next)
  if (check.errors.length)
    throw new Error(`更新后的记录不符合 schema（脚本缺陷）：\n  ${check.errors.join('\n  ')}`)
  return next
}

/** 默认输出路径：target/perf/<date>-<os>.json，已存在时加序号（基线另行用 --out 指定 scripts/perf/baselines/ 下的文件） */
export function defaultOut(os, now = new Date()) {
  const date = now.toISOString().slice(0, 10)
  const base = join(ROOT, 'target', 'perf', `${date}-${os}`)
  if (!existsSync(`${base}.json`)) return `${base}.json`
  for (let i = 2; ; i++) if (!existsSync(`${base}-${i}.json`)) return `${base}-${i}.json`
}

function print(record) {
  const i = record.idle
  if (i) {
    const dash = (v, unit) => (v === null ? '—' : `${v}${unit}`)
    const wv =
      i.rss_webview === null
        ? '—（没有 WebView 进程数据）'
        : `${i.rss_webview} MiB（峰值 ${i.rss_webview_max} MiB，${i.webview_processes} 个进程，归属 ${i.webview_attribution}）`
    console.log(`空闲 ${i.duration_s} s，${i.samples} 个样本`)
    console.log(`  内存口径      ${i.memory_metric ?? 'rss'}`)
    console.log(`  主进程内存    平均 ${i.rss_main} MiB，峰值 ${i.rss_main_max} MiB`)
    console.log(`  WebView 内存  平均 ${wv}`)
    console.log(
      `  CPU           主进程 ${i.cpu_avg}%（单秒峰值 ${i.cpu_max}%）· WebView ${dash(i.cpu_webview_avg, '%')} · 合计 ${dash(i.cpu_total_avg, '%')}`,
    )
    console.log(
      `  唤醒          主进程 ${dash(i.wakeups, ' 次/s')} · WebView ${dash(i.wakeups_webview, ' 次/s')}${i.wakeups_method ? `（${i.wakeups_method}）` : ''}`,
    )
    const ss = i.session
    if (ss) {
      const state = {
        hot: '回收前',
        recycled: '回收后',
        unloaded: '卸载后',
        mixed: '窗口内发生了回收 / 卸载',
        unknown: '状态未知',
      }[ss.state]
      const events = [
        ...ss.recycles_s.map((t) => `${t} s 回收`),
        ...ss.unloads_s.map((t) => `${t} s 卸载`),
      ]
      const panel = {
        hidden: '面板隐藏着',
        visible: '面板仍在屏幕上（测量无效）',
        unknown: '面板状态未知',
      }[ss.panel ?? 'unknown']
      console.log(
        `  会话          exercise 唤出面板 ${ss.views ? `${ss.views.length} 次（${ss.views.join('、')}）` : `${ss.shown ?? '—'} 次`}；exercise 之后 ${ss.window_s.join('–')} s 采样：${state}${events.length ? `（${events.join('、')}）` : ''}；${panel}`,
      )
      if (ss.hides) {
        const hides = Object.entries(ss.hides).map(([k, n]) => `${k} ×${n}`)
        const clips = Object.entries(ss.clipboard ?? {}).map(([k, n]) => `${k} ${n}`)
        console.log(
          `                隐藏 ${hides.join('、') || '—'}${ss.last_hide_s === null ? '' : `（最后一次在 exercise 结束后 ${ss.last_hide_s} s）`}；剪贴板记录 ${clips.join('、') || '0'}；采样窗口里日志 ${ss.log_lines ?? '—'} 行`,
        )
      }
    }
    for (const n of i.notes) console.log(`  · ${n}`)
  }
  const l = record.latency
  if (l)
    console.log(
      `延迟 prepare→shown p50 ${l.p50} ms，p95 ${l.p95} ms（${l.count} 次，来源 ${l.source}）`,
    )
  for (const [build, s] of Object.entries(record.smoke ?? {})) {
    const wv =
      s.rss_webview === null || s.rss_webview === undefined
        ? ''
        : `，会话后 WebView 合计 ${s.rss_webview} MiB（峰值 ${s.rss_webview_peak} MiB）`
    console.log(
      `冒烟（${build}）托盘就绪 ${s.cold_start_ms} ms，主进程 RSS ${s.rss_main} MiB（峰值 ${s.rss_main_peak} MiB）${wv}，prepare→shown p95 ${s.prepare_to_shown_p95} ms`,
    )
  }
}

function selfTest() {
  const meta = (lines) => ['meta os macos', 'meta build release', ...lines].join('\n')
  // WebView 进程中途增减：只按相邻两个样本里都存在的进程累加 CPU，新进程自带的累计 CPU 不会被当成增量
  const raw = [
    meta([
      'meta webview_attribution responsible',
      'meta wakeups_method top-idlew',
      'meta wakeups_start 1000',
      'meta wakeups_end 1040',
      'meta wv_wakeups_start 11:500,12:10',
      'meta wv_wakeups_end 11:520,12:30,13:999',
    ]),
    'sample 0 30720 1000 11:20480:5000,12:10240:100',
    'sample 1000 30720 1001 11:20480:5010,12:10240:100',
    'sample 2000 31744 1002 11:20480:5020,12:10240:100,13:4096:90000',
    'sample 3000 31744 1003 11:20480:5030,13:4096:90000',
    'sample 4000 31744 1004 -',
  ].join('\n')
  const { meta: m, samples } = parseRaw(raw)
  const idle = idleFromSamples(m, samples)
  assert.equal(idle.samples, 5)
  assert.equal(idle.cpu_avg, 0.1) // 4 ms / 4 s
  // WebView 有连续数据的区间是 0→3 s：30 ms / 3 s = 1%
  assert.equal(idle.cpu_webview_avg, 1)
  assert.equal(idle.cpu_total_avg, 1.1)
  assert.equal(idle.webview_processes, 3)
  assert.equal(idle.rss_webview, 29.5) // 有 WebView 的 4 个样本：(30 + 30 + 34 + 24) / 4 MiB
  assert.equal(idle.rss_webview_max, 34)
  assert.equal(idle.wakeups, 10) // 40 次 / 4 s
  assert.equal(idle.wakeups_webview, 10) // pid 11、12 各 20 次（13 只在结束时出现，不计）/ 4 s
  assert.equal(idle.webview_attribution, 'responsible')
  // 取不到的唤醒（-）与没有 WebView 的样本：记为 null，不是 0
  const none = idleFromSamples(
    parseRaw(meta(['meta wakeups_start -', 'meta wakeups_end -'])).meta,
    parseRaw('sample 0 1024 0 -\nsample 1000 1024 0 -').samples,
  )
  assert.equal(none.wakeups, null)
  assert.equal(none.rss_webview, null)
  assert.equal(none.cpu_webview_avg, null)
  assert.equal(none.cpu_total_avg, null)
  assert.equal(none.webview_attribution, null)
  assert.throws(() => parseRaw('sample 0 1 2 11:3'), /pid/)
  assert.throws(() => parseRaw('sample 0 1 2 - extra'), /样本格式/)

  // 会话信息（--app-log）：exercise 之后的追踪、回收时刻与采样窗口的状态
  const ts = (ms) => new Date(ms).toISOString()
  const logLine = (ms, msg) => `${ts(ms)} INFO  wisp_desktop_lib::x: ${msg}`
  const perfLine = (ms, id, shown) =>
    logLine(
      ms,
      `perf trace=${id} origin=cli session=${id} method=- selection_ms=- prepare_ms=2 ready_ms=8 shown_ms=${shown} first_token_ms=- ready_timeout=false overlay_ms=- selection=- window_ms=- key_ms=- page=warm`,
    )
  const t0 = Date.parse('2026-10-12T00:00:00Z')
  const appLog = [
    logLine(t0 - 30_000, 'startup stage=tray_ready elapsed_ms=150'),
    perfLine(t0 - 20_000, 9, 15), // exercise 之前（另一次运行）：不计
    perfLine(t0 + 2_000, 1, 14),
    logLine(t0 + 2_500, 'continuation without timestamp follows'),
    '  续行',
    perfLine(t0 + 5_000, 2, '-'),
    logLine(t0 + 51_000, 'panel idle 45012 ms after use, recycling webview'),
  ].join('\n')
  const sessMeta = (start, end) =>
    parseRaw(
      meta([`meta exercise_start_ms ${start}`, `meta exercise_end_ms ${end}`, 'meta settle_s 8']),
    ).meta
  const win = (from, to) => [{ t: t0 + 6_000 + from * 1000 }, { t: t0 + 6_000 + to * 1000 }]
  const hot = sessionFromLog(sessMeta(t0, t0 + 6_000), win(8, 38), appLog, 'wisp.log')
  assert.deepEqual(hot, {
    settle_s: 8,
    window_s: [8, 38],
    traces: 2,
    shown: 1,
    recycles_s: [45],
    unloads_s: [],
    state: 'hot',
    // 没有 debug 行：面板状态无从核对（不当作隐藏）
    panel: 'unknown',
    views: null,
    hides: null,
    last_hide_s: null,
    clipboard: null,
    log_lines: 0,
    log: 'wisp.log',
  })
  assert.equal(sessionFromLog(sessMeta(t0, t0 + 6_000), win(60, 120), appLog).state, 'recycled')
  assert.equal(sessionFromLog(sessMeta(t0, t0 + 6_000), win(30, 60), appLog).state, 'mixed')
  const unloaded = `${appLog}\n${logLine(t0 + 90_000, 'panel idle, unloading webview (unload_panel_after_idle_min)')}`
  assert.equal(sessionFromLog(sessMeta(t0, t0 + 6_000), win(100, 120), unloaded).state, 'unloaded')
  const noLog = sessionFromLog(sessMeta(t0, t0 + 6_000), win(8, 38), null, 'wisp.log')
  assert.equal(noLog.state, 'unknown')
  assert.equal(noLog.shown, null)
  assert.equal(noLog.panel, 'unknown')
  assert.equal(sessionFromLog(parseRaw(meta([])).meta, win(8, 38), appLog), null)

  // debug 级别的面板事件：exercise 的最后状态（失焦先隐藏后，下一次 --trigger 会把面板重新打开）
  const dbg = (ms, msg, target = 'windows::panel') =>
    `${ts(ms)} DEBUG wisp_desktop_lib::${target}: ${msg}`
  const prep = (ms, id, view) =>
    dbg(
      ms,
      `panel_prepare session=${id} view=${view} placement=Center width=420 max_height=600`,
      'windows::manager',
    )
  const front = (ms, id, reason) =>
    dbg(ms, `panel hide session=${id} reason=${reason} origin=frontend`, 'ipc::cmd_window')
  const clip = (ms, id, kind) =>
    dbg(ms, `clipboard item inserted id=${id} kind=${kind}`, 'clipboard_svc::engine')
  const debugLog = (extra = []) =>
    [
      logLine(t0 + 100, 'clipboard watch installed'),
      clip(t0 + 200, 1, 'Text'),
      clip(t0 + 300, 2, 'Link'),
      clip(t0 + 400, 3, 'Image'),
      prep(t0 + 1_000, 1, 'Input'),
      dbg(t0 + 1_050, 'panel session=1 shown 48 ms after prepare'),
      dbg(t0 + 1_900, 'panel session=1 key=false'),
      dbg(t0 + 1_901, 'panel hide session=1 reason=Blur origin=rust'),
      dbg(t0 + 1_902, 'panel session=1 ended'),
      front(t0 + 2_200, 1, 'Blur'),
      prep(t0 + 2_600, 2, 'Input'),
      dbg(t0 + 2_650, 'panel session=2 shown 40 ms after prepare'),
      dbg(t0 + 3_400, 'panel hide session=2 reason=Toggle origin=rust'),
      dbg(t0 + 3_401, 'panel session=2 ended'),
      front(t0 + 3_700, 2, 'Toggle'),
      prep(t0 + 4_000, 3, 'Clipboard'),
      dbg(t0 + 4_060, 'panel session=3 shown 51 ms after prepare'),
      ...extra,
    ].join('\n')
  const closed = debugLog([
    dbg(t0 + 5_000, 'panel hide session=3 reason=Toggle origin=rust'),
    dbg(t0 + 5_001, 'panel session=3 ended'),
    front(t0 + 5_300, 3, 'Toggle'),
  ])
  const ok1 = sessionFromLog(sessMeta(t0, t0 + 6_000), win(8, 38), closed)
  assert.equal(ok1.panel, 'hidden')
  assert.deepEqual(ok1.views, ['input', 'input', 'clipboard'])
  assert.deepEqual(ok1.hides, { blur: 1, toggle: 2 })
  assert.equal(ok1.last_hide_s, -1)
  assert.deepEqual(ok1.clipboard, { text: 1, link: 1, image: 1 })
  // 内容与历史里已有的相同：应用只置顶（touched），也算记录了这次复制（面板回收对照的 recycled 段）
  const touched = sessionFromLog(
    sessMeta(t0, t0 + 6_000),
    win(8, 38),
    `${closed}\n${dbg(t0 + 5_500, 'clipboard item touched id=2', 'clipboard_svc::engine')}`,
  )
  assert.deepEqual(touched.clipboard, { text: 1, link: 1, image: 1, touched: 1 })
  // 最后一个会话没有隐藏：采样的是可见的面板
  const stillOpen = sessionFromLog(sessMeta(t0, t0 + 6_000), win(8, 38), debugLog())
  assert.equal(stillOpen.panel, 'visible')
  assert.deepEqual(stillOpen.hides, { blur: 1, toggle: 1 })
  // 窗口开始前已隐藏、采样窗口里又上屏了一次：同样是 visible；窗口里写出的日志行计数
  const reopened = sessionFromLog(
    sessMeta(t0, t0 + 6_000),
    win(8, 38),
    `${closed}\n${dbg(t0 + 20_000, 'panel session=4 shown 30 ms after prepare')}`,
  )
  assert.equal(reopened.panel, 'visible')
  assert.equal(reopened.log_lines, 1)
  // 换视图的 Retrigger 没有原因日志（unlogged）；没有 prepare 行的会话视图记为 unknown
  const retrigger = sessionFromLog(
    sessMeta(t0, t0 + 6_000),
    win(8, 38),
    [
      dbg(t0 + 1_000, 'panel session=1 shown 40 ms after prepare'),
      dbg(t0 + 2_000, 'panel session=1 ended'),
      dbg(t0 + 2_050, 'panel session=2 shown 40 ms after prepare'),
      front(t0 + 3_000, 2, 'Escape'),
      dbg(t0 + 3_001, 'panel session=2 ended'),
    ].join('\n'),
  )
  assert.deepEqual(retrigger.hides, { unlogged: 1, escape: 1 })
  assert.deepEqual(retrigger.views, ['unknown', 'unknown'])
  assert.equal(retrigger.panel, 'hidden')
  // 整条 buildRecord：带 debug 面板事件的会话信息符合 schema
  const tmpDir = mkdtempSync(join(tmpdir(), 'wisp-record-'))
  try {
    const logPath = join(tmpDir, 'wisp.log')
    writeFileSync(logPath, closed)
    const rawSess = [
      meta([
        `meta exercise_start_ms ${t0}`,
        `meta exercise_end_ms ${t0 + 6_000}`,
        'meta settle_s 8',
      ]),
      `sample ${t0 + 14_000} 20480 100 -`,
      `sample ${t0 + 15_000} 20480 101 -`,
    ].join('\n')
    const built = buildRecord({ rawText: rawSess, appLog: logPath })
    assert.equal(built.idle.session.panel, 'hidden')
    assert.equal(built.idle.launch_env, null) // 日志里没有预检行
    writeFileSync(
      logPath,
      `${logLine(t0 - 500, 'preflight transparent_windows=true wayland=false shortcut_plugin=true hotkey_backend=plugin launch_env=true')}\n${closed}`,
    )
    assert.equal(buildRecord({ rawText: rawSess, appLog: logPath }).idle.launch_env, true)
    assert.deepEqual(built.idle.session.views, ['input', 'input', 'clipboard'])
  } finally {
    rmSync(tmpDir, { recursive: true, force: true })
  }
  assert.equal(logSince(appLog, t0 + 2_400).split('\n').length, 4) // 续行跟随上一行
  // 更新检查的说明：以前的运行不计；落在采样窗口里的单独指出
  const upd = [
    logLine(t0 - 100_000, 'update check trigger=Automatic'),
    logLine(t0 + 30_000, 'update check trigger=Automatic'),
  ].join('\n')
  assert.match(
    updateCheckNote(upd, t0, win(20, 40)),
    /发生 1 次（启动后 30 s），其中 1 次落在采样窗口里/,
  )
  assert.match(updateCheckNote(upd, t0, win(40, 60)), /都不在采样窗口里/)
  assert.match(updateCheckNote(appLog, t0, win(8, 38)), /没有发生/)
  assert.equal(updateCheckNote(upd, null, win(8, 38)), null)
  assert.equal(logLineMs('[INFO] 没有时间戳'), null)
  // 启动环境（idle.launch_env）：只认这次启动之后的第一条预检行；其他平台、读不到日志、旧版本没有这一项时为 null
  const pre = (ms, on) =>
    logLine(
      ms,
      `preflight transparent_windows=true wayland=false shortcut_plugin=true hotkey_backend=plugin launch_env=${on}`,
    )
  const envLog = [pre(t0 - 60_000, true), pre(t0 + 50, false)].join('\n')
  const launched = parseRaw(meta([`meta launch_ms ${t0}`])).meta
  assert.equal(launchEnvFromLog(launched, envLog), false)
  assert.equal(launchEnvFromLog(launched, pre(t0 + 50, true)), true)
  assert.equal(launchEnvFromLog(parseRaw(meta([])).meta, envLog), false) // 没有 launch_ms：最后一次启动
  assert.equal(launchEnvFromLog(launched, appLog), null)
  assert.equal(launchEnvFromLog(launched, null), null)
  assert.equal(
    launchEnvFromLog(parseRaw(['meta os linux', `meta launch_ms ${t0}`].join('\n')).meta, envLog),
    null,
  )

  // 用户活动（idle.activity）：各档时长、复制次数、不能用 exercise 解释的活跃档
  const tierLine = (ms, tier) =>
    `${ts(ms)} DEBUG wisp_platform::macos::poll_gate: clipboard polling tier ${tier}`
  const macMeta = (extra = []) =>
    parseRaw(meta(['meta os macos', `meta launch_ms ${t0}`, ...extra])).meta
  const span = (from, to) => [{ t: t0 + from * 1000 }, { t: t0 + to * 1000 }]
  const busyLog = [
    logLine(t0 + 100, 'clipboard watch installed'),
    tierLine(t0 + 2_000, 'Quiet'),
    tierLine(t0 + 40_000, 'Active'),
    clip(t0 + 41_000, 7, 'Text'),
    clip(t0 + 70_000, 8, 'Text'),
    tierLine(t0 + 100_000, 'Quiet'),
    `${ts(t0 + 110_000)} DEBUG wisp_platform::macos::poll_gate: clipboard polling slowed: screens idle`,
    `${ts(t0 + 115_000)} DEBUG wisp_platform::macos::poll_gate: clipboard polling resumed`,
  ].join('\n')
  const act = activityFromLog(macMeta(), span(30, 120), busyLog)
  assert.deepEqual(act, {
    tiers_s: { active: 60, quiet: 25, away: 0, screens_idle: 5 },
    copies: 2,
    user_active_s: 60,
  })
  assert.match(
    activityNote(act),
    /活跃 60 s \/ 安静 25 s \/ 离开 0 s \/ 屏幕空闲 5 s，复制 2 次——有人在用/,
  )
  // 用户不在：一直在慢档、没有复制
  const quietLog = [tierLine(t0 + 2_000, 'Quiet'), tierLine(t0 + 302_000, 'Away')].join('\n')
  const away = activityFromLog(macMeta(), span(400, 1000), quietLog)
  assert.deepEqual(away, {
    tiers_s: { active: 0, quiet: 0, away: 600, screens_idle: 0 },
    copies: 0,
    user_active_s: 0,
  })
  assert.doesNotMatch(activityNote(away), /有人在用/)
  // exercise 之后 32 s 以内的活跃档是 exercise 自己造成的，不算有人在用
  const afterEx = activityFromLog(
    macMeta([`meta exercise_start_ms ${t0 + 30_000}`, `meta exercise_end_ms ${t0 + 40_000}`]),
    span(48, 80),
    [
      tierLine(t0 + 2_000, 'Quiet'),
      tierLine(t0 + 31_000, 'Active'),
      tierLine(t0 + 75_000, 'Quiet'),
    ].join('\n'),
  )
  assert.equal(afterEx.tiers_s.active, 27)
  assert.equal(afterEx.user_active_s, 3)
  // 没有 debug 行：无从判断；其他平台没有分档，只数复制
  assert.equal(activityFromLog(macMeta(), span(30, 120), appLog), null)
  const linux = activityFromLog(
    parseRaw(meta(['meta os linux', `meta launch_ms ${t0}`])).meta,
    span(30, 120),
    busyLog
      .split('\n')
      .filter((l) => !l.includes('poll_gate'))
      .join('\n'),
  )
  assert.deepEqual(linux, { tiers_s: null, copies: 2, user_active_s: null })
  // 重复复制历史里已有的内容（应用只置顶，日志 touched）同样是有人在用：与 session.clipboard 一样计入 copies
  const touchedOnly = activityFromLog(
    parseRaw(meta(['meta os windows', `meta launch_ms ${t0}`])).meta,
    span(30, 120),
    [
      logLine(t0 + 100, 'clipboard watch installed'),
      dbg(t0 + 50_000, 'clipboard item touched id=3', 'clipboard_svc::engine'),
      dbg(t0 + 60_000, 'clipboard item touched id=3', 'clipboard_svc::engine'),
      dbg(t0 + 200_000, 'clipboard item touched id=4', 'clipboard_svc::engine'),
    ].join('\n'),
  )
  assert.deepEqual(touchedOnly, { tiers_s: null, copies: 2, user_active_s: null })
  assert.match(activityNote(touchedOnly), /复制 2 次——有人在用/)

  // --update：只替换冒烟报告对应构建类型的段，空闲段与另一种构建类型的冒烟段保留
  const example = JSON.parse(
    readFileSync(new URL('./examples/perf-record.example.json', import.meta.url), 'utf8'),
  )
  const smokePath = fileURLToPath(new URL('./examples/smoke-report.example.json', import.meta.url))
  const updated = updateRecord(
    { ...example, smoke: null },
    [smokePath],
    new Date('2026-10-12T00:00:00Z'),
  )
  assert.deepEqual(updated.idle, example.idle)
  assert.equal(updated.smoke.release.rss_webview, 50.5)
  assert.equal(updated.smoke.release.memory_metric, 'phys_footprint')
  assert.equal(updated.smoke.debug, undefined)
  assert.ok(updated.notes.at(-1).startsWith('2026-10-12：record.mjs --update 并入 release'))
  const kept = updateRecord(example, [smokePath])
  assert.deepEqual(kept.smoke.debug, example.smoke.debug)
  assert.throws(() => updateRecord({ ...example, os: 'linux' }, [smokePath]), /macos/)
  assert.throws(() => updateRecord(example, []), /--smoke/)
  ok(
    'record 自检通过（主进程 / WebView 的 CPU、RSS、唤醒口径；会话信息；启动环境；--update 合并冒烟段）',
  )
}

function main() {
  if (process.argv.includes('--self-test')) return selfTest()
  const { values } = parseArgs({
    options: {
      raw: { type: 'string' },
      update: { type: 'string' },
      out: { type: 'string' },
      'latency-log': { type: 'string' },
      'app-log': { type: 'string' },
      smoke: { type: 'string', multiple: true, default: [] },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help || (!values.raw && !values.update)) {
    console.log(
      '用法：node scripts/perf/record.mjs --raw <样本文件> [--out <文件>|-] [--latency-log <日志>] [--app-log <日志>] [--smoke <报告>]…\n' +
        '      node scripts/perf/record.mjs --update <记录> --smoke <报告>… [--out <文件>|-]',
    )
    if (!values.help) throw new Error('缺少 --raw 或 --update')
    return
  }
  if (values.update) {
    if (values.raw || values['latency-log'] || values['app-log'])
      throw new Error('--update 只合并冒烟段，不能与 --raw / --latency-log / --app-log 同时使用')
    const next = updateRecord(readJson(values.update), values.smoke)
    print(next)
    const json = `${JSON.stringify(next, null, 2)}\n`
    if (values.out === '-') {
      process.stdout.write(json)
      return
    }
    const out = values.out ?? values.update
    writeFileSync(out, json)
    ok(`已更新 ${out.startsWith(ROOT) ? rel(out) : out}（schema 校验通过）`)
    return
  }
  const record = buildRecord({
    rawText: readFileSync(values.raw, 'utf8'),
    latencyLog: values['latency-log'],
    appLog: values['app-log'],
    smokePaths: values.smoke,
  })
  print(record)
  const json = `${JSON.stringify(record, null, 2)}\n`
  if (values.out === '-') {
    process.stdout.write(json)
    return
  }
  const out = values.out ?? defaultOut(record.os)
  mkdirSync(dirname(out), { recursive: true })
  writeFileSync(out, json)
  ok(`已写入 ${out.startsWith(ROOT) ? rel(out) : out}（schema 校验通过）`)
}

if (isMain(import.meta.url)) await run(main)
