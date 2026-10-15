/**
 * scripts/perf/lib.mjs：性能脚本共用的统计、日志解析与 JSON Schema 校验。
 *
 * 只依赖 Node 内置模块。格式定义见同目录 README.md 与 `*.schema.json`。
 */
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))

/** 性能记录的 schema 标识 */
export const PERF_SCHEMA = 'wisp.perf/1'
/** 冒烟报告的 schema 标识：应用（`app::smoke`）写出的数字版本号 */
export const SMOKE_SCHEMA = 1
/** 性能记录与基线里冒烟摘要的分组键（release / debug 口径不同，分开保存与比较） */
export const BUILDS = ['release', 'debug']

let schemas
/** 读取两份 JSON Schema（惰性） */
export function loadSchemas() {
  schemas ??= {
    perf: JSON.parse(readFileSync(join(HERE, 'perf-record.schema.json'), 'utf8')),
    smoke: JSON.parse(readFileSync(join(HERE, 'smoke-report.schema.json'), 'utf8')),
  }
  return schemas
}

// ---------------------------------------------------------------------------
// 统计

/**
 * 最近秩法（nearest-rank）分位数：排序后取第 ⌈p·n⌉ 个值。样本为空时返回 null。
 * 不做插值：p95 一定是某个真实样本，100 个样本时就是第 95 小的值。
 */
export function percentile(values, p) {
  const xs = values.filter((v) => Number.isFinite(v)).toSorted((a, b) => a - b)
  if (xs.length === 0) return null
  const rank = Math.min(xs.length, Math.max(1, Math.ceil(p * xs.length)))
  return xs[rank - 1]
}

/** 平均值；样本为空时返回 null */
export function mean(values) {
  const xs = values.filter((v) => Number.isFinite(v))
  return xs.length === 0 ? null : xs.reduce((a, b) => a + b, 0) / xs.length
}

/** 保留 n 位小数（null 原样返回） */
export function round(v, n = 2) {
  if (v === null || v === undefined || !Number.isFinite(v)) return v ?? null
  const k = 10 ** n
  return Math.round(v * k) / k
}

/** 一组延迟样本的摘要 */
export function summarize(values) {
  const xs = values.filter((v) => Number.isFinite(v))
  return {
    count: xs.length,
    p50: round(percentile(xs, 0.5)),
    p95: round(percentile(xs, 0.95)),
    max: xs.length ? round(Math.max(...xs)) : null,
    mean: round(mean(xs)),
  }
}

// ---------------------------------------------------------------------------
// 延迟日志（README「延迟日志格式」）

/** `键=值` 中的值：`-` 为 null，true / false 为布尔，能解析成数字的为数字 */
function kvValue(v) {
  if (v === '-') return null
  if (v === 'true') return true
  if (v === 'false') return false
  const n = Number(v)
  return v !== '' && Number.isFinite(n) ? n : v
}

/**
 * 从日志文本里取出每次唤出的追踪（LatencyMarks 的字段）。识别两种行：
 * - 应用 `PerfMarks::finish` 写的行：`… perf trace=3 origin=hotkey session=7 method=- selection_ms=- prepare_ms=2
 *   ready_ms=9 shown_ms=21 first_token_ms=310 ready_timeout=false`（没有到达的阶段为 `-`）；
 * - `… perf latency {"session_id":1,"prepare_ms":2,"shown_ms":31,…}`（LatencyMarks 的 JSON 原样写在后面）。
 * 其他行（包括 `perf paste …`、`startup stage=…`）忽略。返回 `{ marks, skipped }`：marks 是解析出的全部追踪
 * （面板没有显示的追踪 shown_ms 为 null），skipped 是看起来是追踪、但无法解析的行数。
 */
export function parseLatencyLog(text) {
  const marks = []
  let skipped = 0
  // 第几次启动（「启动后第一次唤出」按启动分组）：日志是追加写的，一个文件里常有多次启动。
  // - 文件里有 `startup stage=tray_ready` 行时**只**以它分界（行是追踪结束时才写的：长的截图追踪可能晚于之后的追踪
  //   写出，trace id 的先后不能说明换了一次启动）；
  // - 整个文件一行 stage 都没有时（日志被截取过）才按 trace id 兜底：同一次启动里 trace id 不重复，**同一个 id 再次
  //   出现**才算换了一次启动（不会把晚写出的 trace=1 误判成新的启动；代价是截取得很碎的日志可能少分出几次启动）
  const lines = text.split(/\r?\n/)
  const hasStage = lines.some((line) => STAGE_LINE.test(line))
  let launch = 0
  let seenIds = new Set()
  let sawStartup = false
  for (const line of lines) {
    if (STAGE_LINE.test(line)) {
      if (sawStartup || marks.length) launch++
      sawStartup = true
      continue
    }
    let m = null
    const json = /\bperf latency (\{.*\})\s*$/.exec(line)
    if (json) {
      try {
        m = JSON.parse(json[1])
      } catch {
        m = null
      }
    } else if (/\bperf\b.*\bshown_ms=/.test(line)) {
      m = {}
      const rest = line.slice(line.search(/\bperf\b/))
      for (const [, k, v] of rest.matchAll(/\b([a-z_]+)=(\S*)/g)) m[k] = kvValue(v)
    } else {
      continue
    }
    if (m && typeof m === 'object' && !Array.isArray(m) && 'shown_ms' in m && 'prepare_ms' in m) {
      if (!hasStage && Number.isFinite(m.trace)) {
        if (seenIds.has(m.trace)) {
          launch++
          seenIds = new Set()
        }
        seenIds.add(m.trace)
      }
      m.launch = launch
      marks.push(m)
    } else {
      skipped++
    }
  }
  return { marks, skipped }
}

const STAGE_LINE = /\bstartup stage=tray_ready\b/

/** 追踪里是否有可用的 prepare→shown（面板显示了，且 prepare 有埋点） */
function hasShown(m) {
  return Number.isFinite(m.shown_ms) && Number.isFinite(m.prepare_ms)
}

/** perf 行 `page=` 的取值：热页面 / 回收出来的新页面 / 空闲卸载之后重建的页面（冷唤出） */
export const PAGES = ['warm', 'recycled', 'cold']

/** 一组追踪的 prepare→shown 摘要与 ready 超时次数（没有样本时分位数为 null） */
function pageSummary(list) {
  const { count, p50, p95, max } = summarize(
    list.filter(hasShown).map((m) => m.shown_ms - m.prepare_ms),
  )
  return {
    count,
    p50,
    p95,
    max,
    ready_timeouts: list.filter((m) => m.ready_timeout === true).length,
  }
}

/** 两个阶段之差（`to − from`，ms）的 p50 / p95 / max；两个阶段都到达的追踪才计入 */
function stageSplit(marks, from, to) {
  const { count, p50, p95, max } = summarize(
    marks
      .filter((m) => Number.isFinite(m[from]) && Number.isFinite(m[to]))
      .map((m) => Math.max(0, m[to] - m[from])),
  )
  return { count, p50, p95, max }
}

/**
 * LatencyMarks[] → perf 记录的 latency 段（主指标：prepare→shown；面板没有显示的追踪不计入分位数）。
 * ready 超时之后又显示出来的会话照样计入分位数（Rust 侧超时后继续等 Shown 才结束追踪）；截图流程的
 * `overlay_ms`（触发 → 第一块屏的 Overlay 上屏，≤ 150 ms）另算 p50 / p95。另外：
 * - `pages`：按 `page=` 分组的 prepare→shown 与 ready 超时。**冷唤出（page=cold，空闲卸载后重建页面，≤ 150 ms）不计入
 *   主指标**（主指标的 100 ms 预算针对热 / 回收页面，冷唤出有自己的预算）；没有 `page=` 的旧日志全部计入主指标；
 * - `split`：ready 之后原生窗口上屏（`window_ms − ready_ms`）、成为 key（`key_ms − ready_ms`）与 ready→shown
 *   （输入视图 ready→shown 的 Rust 侧拆分，目标 ≤ 100 ms）；
 * - `first_show`：每次启动后的第一次唤出（启动后第一次唤出同样要满足唤出延迟预算，40 ms 的 ready 兜底超时在这里
 *   最常见）：次数、其中 ready 超时的次数、prepare→shown 的最大值。
 */
export function latencyFromMarks(marks) {
  const warmish = marks.filter((m) => m.page !== 'cold')
  const shown = warmish.filter(hasShown)
  const firstToken = marks.map((m) => m.first_token_ms).filter((v) => Number.isFinite(v))
  const overlay = marks.map((m) => m.overlay_ms).filter((v) => Number.isFinite(v))
  const ov = summarize(overlay)
  const pages = {}
  for (const page of PAGES) {
    const list = marks.filter((m) => m.page === page)
    if (list.length) pages[page] = pageSummary(list)
  }
  // 冷唤出另报「卡片上屏」（prepare→window_ms：原生外壳上屏；交接给新会话时记在交接时刻）——
  // 冷唤出的 150 ms 预算按它判（含原生首帧掩盖手段），prepare→shown（内容首帧）照常在 p50 / p95 里
  if (pages.cold)
    pages.cold.window = stageSplit(
      marks.filter((m) => m.page === 'cold'),
      'prepare_ms',
      'window_ms',
    )
  // 每次启动的第一次面板唤出：有 prepare 埋点（截图流程在 Overlay 阶段没有 prepare，不算）且 trace id 最小的那条——
  // 行是追踪结束时才写的，真正的第一次可能晚于之后的追踪写出，不能按行序取；没有 trace id 的旧格式按行序
  const byLaunch = new Map()
  for (const m of marks) {
    if (!Number.isFinite(m.prepare_ms)) continue
    const launch = m.launch ?? 0
    const cur = byLaunch.get(launch)
    const earlier =
      cur === undefined ||
      (Number.isFinite(m.trace) && Number.isFinite(cur.trace) && m.trace < cur.trace)
    if (earlier) byLaunch.set(launch, m)
  }
  const firsts = [...byLaunch.values()]
  const firstShown = firsts.filter(hasShown).map((m) => m.shown_ms - m.prepare_ms)
  return {
    source: 'log',
    metric: 'prepare_to_shown_ms',
    ...summarize(shown.map((m) => m.shown_ms - m.prepare_ms)),
    ready_timeouts: marks.filter((m) => m.ready_timeout === true).length,
    first_token_p50: round(percentile(firstToken, 0.5)),
    first_token_p95: round(percentile(firstToken, 0.95)),
    overlay_count: ov.count,
    overlay_p50: ov.p50,
    overlay_p95: ov.p95,
    overlay_max: ov.max,
    pages,
    split: {
      ready_to_window: stageSplit(marks, 'ready_ms', 'window_ms'),
      ready_to_key: stageSplit(marks, 'ready_ms', 'key_ms'),
      ready_to_shown: stageSplit(marks, 'ready_ms', 'shown_ms'),
    },
    first_show: {
      count: firsts.length,
      ready_timeouts: firsts.filter((m) => m.ready_timeout === true).length,
      not_shown: firsts.filter((m) => !hasShown(m)).length,
      max: firstShown.length ? round(Math.max(...firstShown)) : null,
    },
  }
}

/** ready 超时的追踪里最终显示出来的那些：[触发→显示 ms]（单独列出） */
export function timedOutShown(marks) {
  return marks
    .filter((m) => m.ready_timeout === true && Number.isFinite(m.shown_ms))
    .map((m) => m.shown_ms)
}

// ---------------------------------------------------------------------------
// 应用日志的时间戳与面板事件（app::logging 的格式：`2026-10-10T07:31:02.123Z DEBUG wisp_desktop_lib::…: 消息`，UTC、
// 毫秒精度，级别左对齐占 5 列）。会话后测量（record.mjs 的 idle.session、exercise.mjs 的状态核对）共用

/** 日志行开头的时间戳 → Unix 毫秒；没有时间戳（续行、其他格式）时为 null */
export function logLineMs(line) {
  const m = /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z)\s/.exec(line)
  return m ? Date.parse(m[1]) : null
}

/** 只保留时间戳不早于 sinceMs 的行（没有时间戳的续行跟随上一行的取舍） */
export function logSince(text, sinceMs) {
  let keep = false
  const out = []
  for (const line of text.split(/\r?\n/)) {
    const t = logLineMs(line)
    if (t !== null) keep = t >= sinceMs
    if (keep) out.push(line)
  }
  return out.join('\n')
}

/**
 * 面板与剪贴板的 debug 级别日志（`advanced.log_level = debug` 时才写出；measure-*.sh 在隔离 HOME 里预置）：
 * - `windows::manager`：`panel_prepare session=N view=Input …`（准备唤出，视图为 PanelView 的 Debug 名）；
 * - `windows::panel`：`panel session=N shown X ms after prepare`（上屏）、`panel session=N ended`（开始隐藏：Toggle / 失焦 /
 *   Esc / 换视图的 Retrigger… 每个会话只记一次）；
 * - `panel hide session=N reason=Toggle origin=rust|frontend`（隐藏的原因：rust 是请求、frontend 是退出动画播完）；
 * - `clipboard_svc::engine`：`clipboard item inserted id=N kind=Text`（剪贴板新条目）、`clipboard item touched id=N`（复制的
 *   内容与历史里已有的一条相同：应用照常记录了这次复制，只把那一条置顶，不新增条目——记为伪类型 `touched`）。
 */
const PANEL_EVENTS = [
  [
    /\bpanel_prepare session=(\d+) view=(\w+)/,
    (m) => ({ kind: 'prepare', session: Number(m[1]), view: snake(m[2]) }),
  ],
  [
    /\bpanel session=(\d+) shown \d+ ms after prepare\b/,
    (m) => ({ kind: 'shown', session: Number(m[1]) }),
  ],
  [/\bpanel session=(\d+) ended\b/, (m) => ({ kind: 'ended', session: Number(m[1]) })],
  [
    /\bpanel hide session=(\d+) reason=(\w+) origin=(\w+)/,
    (m) => ({ kind: 'hide', session: Number(m[1]), reason: m[2], origin: m[3] }),
  ],
  [/\bclipboard item inserted id=\d+ kind=(\w+)/, (m) => ({ kind: 'clip', clip: snake(m[1]) })],
  [/\bclipboard item touched id=\d+/, () => ({ kind: 'clip', clip: CLIP_TOUCHED })],
]

/**
 * `PanelTracker.clips` 里的伪类型：复制的内容与历史里已有的条目相同、只置顶（不是 ClipKind，不会与真实类型重名）。
 * 历史没有清空时（Windows 上测量用真实用户目录，上一段测量灌入的同样内容还在）exercise 的写入全部落在这里
 */
export const CLIP_TOUCHED = 'touched'

/** `RichText` → `rich_text`（与 IPC 的 snake_case 一致） */
function snake(name) {
  return name.replace(/(?<=[a-z0-9])([A-Z])/g, '_$1').toLowerCase()
}

/**
 * 日志文本 → `{ events, debug }`：events 为带时刻（Unix 毫秒）的面板 / 剪贴板事件（按行序）；debug 表示文本里有没有
 * debug 级别的行（没有时面板事件一定是空的，「没有事件」不能当作「面板没显示」）
 */
export function parsePanelEvents(text) {
  const events = []
  let debug = false
  for (const line of text.split(/\r?\n/)) {
    const t = logLineMs(line)
    if (t === null) continue
    if (!debug && /^\S+ DEBUG /.test(line)) debug = true
    for (const [re, make] of PANEL_EVENTS) {
      const m = re.exec(line)
      if (m) {
        events.push({ t, ...make(m) })
        break
      }
    }
  }
  return { events, debug }
}

/**
 * 面板事件的汇总：每个上屏的会话（视图、上屏 / 开始隐藏的时刻、隐藏原因），某一时刻是否有面板在屏幕上，剪贴板记录（新条目按类型、置顶的已有条目记为 `touched`）。
 * 会话 id 单调递增；「在屏幕上」= 上屏了、还没有开始隐藏。准备了但还没上屏（也没被取代）的会话记为 pending
 */
export class PanelTracker {
  constructor() {
    this.sessions = new Map()
    this.clips = {}
    this.order = []
  }

  /** 按时间顺序喂入事件 */
  add(e) {
    if (e.kind === 'clip') {
      this.clips[e.clip] = (this.clips[e.clip] ?? 0) + 1
      return
    }
    let s = this.sessions.get(e.session)
    if (!s) {
      s = { id: e.session, view: null, prepared: null, shown: null, ended: null, reason: null }
      this.sessions.set(e.session, s)
    }
    if (e.kind === 'prepare') {
      s.view = e.view
      s.prepared ??= e.t
    } else if (e.kind === 'shown') {
      if (s.shown === null) this.order.push(s)
      s.shown ??= e.t
    } else if (e.kind === 'ended') {
      s.ended ??= e.t
    } else if (e.kind === 'hide') {
      // 退出动画播完（frontend）的原因优先于请求（rust）
      if (s.reason === null || e.origin === 'frontend') s.reason = e.reason
    }
  }

  /** 上屏过的会话（按上屏顺序） */
  shown() {
    return this.order
  }

  /** t 时刻在屏幕上的会话（上屏不晚于 t、开始隐藏晚于 t 或还没隐藏） */
  visibleAt(t = Infinity) {
    return this.order.filter((s) => s.shown <= t && (s.ended === null || s.ended > t))
  }

  /** 已准备、还没上屏也没结束的会话（唤出正在进行） */
  pending() {
    return [...this.sessions.values()].filter(
      (s) => s.prepared !== null && s.shown === null && s.ended === null,
    )
  }

  /**
   * 上屏过的会话的隐藏原因计数（snake_case）。Rust 内部直接隐藏、不经 hide_request 的路径（换视图的 Retrigger、粘贴）没有
   * 原因日志，记为 unlogged；还没隐藏的不计
   */
  hideReasons() {
    const out = {}
    for (const s of this.order) {
      if (s.ended === null) continue
      const r = s.reason ? snake(s.reason) : 'unlogged'
      out[r] = (out[r] ?? 0) + 1
    }
    return out
  }

  /** 应用记录到的剪贴板变化总数：新条目 + 置顶的已有条目（`touched`） */
  clipTotal() {
    return Object.values(this.clips).reduce((a, b) => a + b, 0)
  }

  /** 其中置顶的已有条目数 */
  clipTouched() {
    return this.clips[CLIP_TOUCHED] ?? 0
  }
}

// ---------------------------------------------------------------------------
// 冒烟报告（应用 `app::smoke` 的格式，README「冒烟报告」）

/** 冒烟报告的构建类型 */
export function smokeBuild(report) {
  return report.debug_build ? 'debug' : 'release'
}

const kibToMib = (v) => (Number.isFinite(v) ? round(v / 1024, 1) : null)

/** 冒烟报告 → perf 记录的 latency 段（冒烟报告只给出 p50 / p95 / max 汇总） */
export function latencyFromSmoke(report) {
  const t = report.triggers
  const s = t.prepare_to_shown_ms
  return {
    source: 'smoke',
    metric: 'prepare_to_shown_ms',
    count: t.shown,
    p50: s.p50,
    p95: s.p95,
    max: s.max,
    mean: null,
    ready_timeouts: t.ready_timeouts,
    first_token_p50: null,
    first_token_p95: null,
  }
}

/** 冒烟报告 → perf 记录 / 基线里 `smoke.<build>` 的摘要（RSS 换算为 MiB） */
export function smokeSummary(report) {
  const kb = report.rss_kb
  const all = [kb.after_startup, kb.after_triggers, kb.end].filter((v) => Number.isFinite(v))
  return {
    ok: report.ok,
    cold_start_ms: report.startup.tray_ready_ms,
    panel_prewarm_ms: report.panel_prewarm_ms,
    rss_main: kibToMib(kb.end),
    rss_main_peak: all.length ? kibToMib(Math.max(...all)) : null,
    prepare_to_shown_p95: report.triggers.prepare_to_shown_ms.p95,
    trigger_to_shown_p95: report.triggers.trigger_to_shown_ms.p95,
    windows_end: report.windows.end,
    // 内存口径：macOS phys_footprint、Windows working_set、Linux rss；旧报告没有这个字段，按 rss
    memory_metric: report.memory_metric ?? 'rss',
    // 录制环境的系统版本（跨系统大版本的 RSS 不可比，smoke-gate 据此把退化比较降为告警）
    os_version: report.os_version ?? null,
    // run-smoke.mjs --webview 采样的 WebView 进程合计——冒烟会话后 10 s 里的中位数与全程峰值（只记录）；没有采样时为 null
    rss_webview: kibToMib(report.webview?.hot_kb ?? null),
    rss_webview_peak: kibToMib(report.webview?.peak_kb ?? null),
    webview_attribution: report.webview?.attribution ?? null,
  }
}

/** 两段记录的内存口径是否可比（旧记录没有 memory_metric，按 rss） */
export function sameMemoryMetric(a, b) {
  return (a?.memory_metric ?? 'rss') === (b?.memory_metric ?? 'rss')
}

// ---------------------------------------------------------------------------
// JSON Schema（draft 2020-12 的子集：type、const、enum、required、properties、additionalProperties、
// items、minimum、exclusiveMinimum、minLength、format: date-time）——够校验本目录的两份 schema

const DATE_TIME = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$/

function typeOf(v) {
  if (v === null) return 'null'
  if (Array.isArray(v)) return 'array'
  if (typeof v === 'number') return Number.isInteger(v) ? 'integer' : 'number'
  return typeof v
}

function typeMatches(actual, expected) {
  return actual === expected || (expected === 'number' && actual === 'integer')
}

/** 校验 value 是否符合 schema；返回错误列表（`路径: 说明`），空数组表示通过 */
export function validate(schema, value, path = '$') {
  const errors = []
  if (schema.const !== undefined && value !== schema.const) {
    errors.push(`${path}: 应为 ${JSON.stringify(schema.const)}`)
    return errors
  }
  if (schema.enum && !schema.enum.includes(value)) {
    errors.push(`${path}: 应为 ${schema.enum.map((e) => JSON.stringify(e)).join(' / ')} 之一`)
    return errors
  }
  if (schema.type) {
    const types = Array.isArray(schema.type) ? schema.type : [schema.type]
    const actual = typeOf(value)
    if (!types.some((t) => typeMatches(actual, t))) {
      errors.push(`${path}: 类型应为 ${types.join(' | ')}，实际为 ${actual}`)
      return errors
    }
  }
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) errors.push(`${path}: 不是有限数`)
    if (schema.minimum !== undefined && value < schema.minimum) {
      errors.push(`${path}: 应 ≥ ${schema.minimum}`)
    }
    if (schema.exclusiveMinimum !== undefined && value <= schema.exclusiveMinimum) {
      errors.push(`${path}: 应 > ${schema.exclusiveMinimum}`)
    }
  }
  if (typeof value === 'string') {
    if (schema.minLength !== undefined && value.length < schema.minLength) {
      errors.push(`${path}: 不能为空`)
    }
    if (
      schema.format === 'date-time' &&
      !(DATE_TIME.test(value) && !Number.isNaN(Date.parse(value)))
    ) {
      errors.push(`${path}: 不是 ISO 8601 时间`)
    }
  }
  if (Array.isArray(value) && schema.items) {
    value.forEach((item, i) => errors.push(...validate(schema.items, item, `${path}[${i}]`)))
  }
  if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
    for (const key of schema.required ?? []) {
      if (!(key in value)) errors.push(`${path}: 缺少字段 ${key}`)
    }
    const props = schema.properties ?? {}
    for (const [key, v] of Object.entries(value)) {
      if (props[key]) errors.push(...validate(props[key], v, `${path}.${key}`))
      else if (schema.additionalProperties === false) errors.push(`${path}: 未知字段 ${key}`)
      else if (typeof schema.additionalProperties === 'object') {
        errors.push(...validate(schema.additionalProperties, v, `${path}.${key}`))
      }
    }
  }
  return errors
}

/** 按 `schema` 字段识别文件类型并校验；返回 `{ kind: 'perf' | 'smoke' | null, errors }` */
export function validateDocument(doc) {
  const { perf, smoke } = loadSchemas()
  if (doc?.schema === PERF_SCHEMA) return { kind: 'perf', errors: validate(perf, doc) }
  if (doc?.schema === SMOKE_SCHEMA) return { kind: 'smoke', errors: validate(smoke, doc) }
  return {
    kind: null,
    errors: [
      `$.schema: 应为 "${PERF_SCHEMA}"（性能记录）或 ${SMOKE_SCHEMA}（冒烟报告），实际为 ${JSON.stringify(doc?.schema)}`,
    ],
  }
}
