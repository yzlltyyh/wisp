#!/usr/bin/env node
/**
 * scripts/perf/smoke-gate.mjs：冒烟报告 / 性能记录的门禁。
 *
 * 用法：
 *   node scripts/perf/smoke-gate.mjs <report.json> [baseline.json] [--ci] [--expect-triggers 100]
 *                                    [--require-webview] [--expect-state hot|recycled|unloaded] [--expect-views v1,v2]
 *                                    [--webview-gate warn|hard]
 *   node scripts/perf/smoke-gate.mjs --self-test
 *
 * report 是冒烟报告（应用的 `app::smoke` 写出的 target/smoke-report.json，`schema: 1`）或性能记录
 * （measure-*.sh 写出的 `wisp.perf/1` 记录）；baseline 是 `scripts/perf/baselines/<os>.json`（同为 `wisp.perf/1`）。
 * 冒烟摘要在基线里按构建类型分开保存（`smoke.release` / `smoke.debug`），只和同构建类型的基线比较。
 *
 * 两种模式（规则表见 README「门禁规则」）：
 * - 默认（本机门禁）：RSS、空闲 CPU（主进程 + WebView）、唤醒、延迟 p95 任一退化 > 10%（且超过测量噪声下限 FLOOR）
 *   或超出预算（含主进程唤醒的绝对预算）即失败；
 * - `--ci`（GitHub runner）：macOS 冒烟的 RSS 退化失败（基线缺少同构建类型的冒烟段也失败），其余退化与预算只告警
 *   （runner 的时间抖动大）。
 * 两种模式下都必过：报告格式合法、流程完成且没有失败项、fake LLM、100 次触发全部显示、0 卡死、ready 超时不超过上限、
 * 启动后第一次唤出没有 ready 超时（不受 gate.max_ready_timeouts 放宽）、设置窗口打开并关闭后没有残留窗口、
 * release 构建的冒烟 RSS 不超过宽松上限。
 *
 * WebView 内存与会话后空闲：
 * - macOS WebView 合计 ≤ 55 MiB，按性能记录的 idle 段判定。measure-*.sh `--exercise`（用一次面板再隐藏）之后的空闲有
 *   两种状态，记录的 `idle.session.state` 由应用日志得出：
 *   - `hot`「回收前（刚用过）」：用完之后、面板回收之前（macOS 回收在隐藏 45 s 后，`windows::manager::panel_recycle_after`）；
 *   - `recycled`「回收后」：回收之后的新页面（用过之后的常驻状态）；`unloaded`「卸载后」：空闲卸载之后；
 *   55 MiB 判**回收后**；回收前按防退化上限 `BUDGET.rss_webview_hot`（100 MiB）判。`--ci` 下 WebView 预算只在
 *   `--webview-gate hard` 时是硬门禁（同时隐含 `--require-webview`；CI 的两个窗口都传 hard）；本机门禁（不带 --ci）照常失败；
 * - 采样窗口里有人在用（`idle.activity`：复制、或不能用 exercise 解释的剪贴板轮询活跃档，record.mjs 从 debug 日志得出）时
 *   空闲 CPU / 唤醒的预算与基线比较降为告警（开发机上的测量不会被误判成退化）；内存照常判定；
 * - `--require-webview`：测量本身必须有效——WebView 进程找到且归属可靠（macOS 必须是经 LaunchServices 的精确归属
 *   `responsible`）、进程数不少于平台下限（macOS 3 个：GPU / Networking / WebContent），否则失败（不会悄悄退化成告警）；
 * - `--expect-state <状态>`：记录必须带会话信息、exercise 确实唤出过面板（日志里有显示过的 perf 追踪）、**采样窗口里面板
 *   已隐藏**（`idle.session.panel = hidden`，由 debug 级别的面板事件得出：失焦先隐藏后再次 --trigger 会重新打开面板，
 *   exercise 结束时面板可能还在屏幕上；日志里没有 debug 行、无法核对时同样失败），且采样窗口的状态与预期一致——回收时长
 *   变了、回收被关掉或 exercise 没生效时直接失败，而不是悄悄测成另一种状态；
 * - `--expect-views <视图,…>`：exercise 之后每个视图（input / clipboard …，`idle.session.views`）都至少上屏过一次——
 *   不规定唤出次数（exercise.mjs 在面板被重新打开时会多关一次，次数因此可能多于视图数），规定用到了哪些视图；
 * - 冒烟报告里 run-smoke.mjs 采的 `webview` 段（`--webview` 打开时才有）只记录不判定：冒烟在 20 s 内连续触发 100 次、
 *   开关设置窗口与 Overlay（它们与面板共用一个 WebContent 进程），读数随 GC 时机摆动，与上面两种状态都不是同一口径；
 * - 性能记录的 latency 段：冷唤出（page=cold，空闲卸载后重建）p95 ≤ 150 ms、ready→窗口上屏 / 成为 key 的 p95 ≤ 100 ms、
 *   每次启动后的第一次唤出不允许 ready 超时（没有显示只告警：真实日志里第一次触发可能因 NotReady 等正当原因不显示）且
 *   prepare→shown ≤ 100 ms、Overlay 上屏 p95 ≤ 150 ms；
 * - 基线存在却没有冒烟段（`smoke: null`）与缺少同构建类型的冒烟段同样处理（本机门禁与 CI 的 macOS 失败）；
 *   基线冒烟段与本次报告的系统大版本不同（例如本机 macOS 26 录的基线、runner 是 macOS 15）时 RSS 退化比较只告警——
 *   跨系统版本的 phys_footprint 不可比，宽松绝对上限照常硬判。
 *
 * 主进程内存门禁（`MallocSpaceEfficient` 悄悄失效时主进程从约 13 MiB 回到约 21 MiB，只看绝对预算拦不住）。只对经
 * LaunchServices 启动的 macOS release 记录（WebView 归属 `responsible`）生效，`--ci` 下同样是硬门禁：
 * - 启动环境：`idle.launch_env`（record.mjs 从应用日志的 `preflight … launch_env=` 读出）必须为 true；false 失败，没有这一项时
 *   `--require-webview` 失败、否则告警；
 * - 从未显示的空闲记录：主进程与基线空闲段的退化比较（基线的空闲段本身带 `idle.session`——runner 基线取的是会话后的
 *   回收后窗口——时只比较主进程，WebView / CPU / 唤醒口径不同、不比较）；会话后窗口（带 `idle.session`）：
 *   主进程与基线的 `gate.session_rss_main` 比较，WebView / CPU / 唤醒不与基线的空闲段比较。基线（顶层 `os_version`）与记录的
 *   系统大版本不同时只告警。CI 的两个会话后窗口传 runner 基线（`baselines/macos-15.json`）。
 */
import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { parseArgs } from 'node:util'
import { appendSummary, error, info, isMain, ok, run, warn } from '../lib/common.mjs'
import {
  BUILDS,
  latencyFromSmoke,
  round,
  sameMemoryMetric,
  smokeBuild,
  smokeSummary,
  validateDocument,
} from './lib.mjs'

/** 性能预算（RSS 单位 MiB，CPU 为单核百分比） */
export const BUDGET = {
  rss_main: { macos: 40, windows: 40, linux: 50 },
  /**
   * WebView 进程合计。macOS 为 55 MiB：WKWebView 的 GPU / Networking / WebContent 三个进程本身就接近 50 MiB，
   * 预算针对用过之后的常驻状态；长时间空闲可由「空闲 N 分钟后卸载面板 WebView」降到接近 0
   */
  rss_webview: { macos: 55, windows: 80, linux: 60 },
  /**
   * macOS「回收前（刚用过）」窗口（隐藏后 45 s 内、面板回收之前）的 WebView 防退化上限。55 MiB 的预算判「回收后」——
   * 用过之后的常驻状态（实测约 53 MiB）；回收前的 45 s 里 WebKit 的 GPU / WebContent 工作集还没交还（实测 82.7–90.6 MiB，
   * 前端与窗口层都没有手段压到 55），这个窗口按实测最大值留约 10% 余量硬判，拦住热状态的内存退化
   */
  rss_webview_hot: { macos: 100 },
  /** 空闲 CPU：主进程 + WebView 合计 */
  cpu_avg: 0.1,
  /**
   * 空闲时主进程的唤醒次数上限（不依赖基线）：macOS 唯一的周期唤醒是剪贴板 changeCount 的 4 Hz 轮询，
   * 另留 1 次/s 给系统事件与测量误差；Windows / X11 空闲时没有周期唤醒，留 0.2 次/s（10 分钟内的偶发唤醒
   * 不超过 120 次）——新增任何一个 ≥ 0.2 Hz 的定时器都会超出
   */
  wakeups_main: { macos: 5, windows: 0.2, linux: 0.2 },
  cold_start_ms: 800,
  prepare_to_shown_p95: 100,
  /**
   * 从触发到面板显示（release）。首次翻译的 `first_translate.shown_ms` 与连续触发的第 1 次 / 最后一次
   * （`triggers.first` / `triggers.last`）都要在预算内（只看 p95 拦不住首次唤出撞上 ready 超时）
   */
  trigger_to_shown_ms: 100,
  /** Overlay 上屏 p95（冒烟用假帧量「建窗口 → 显示」；性能记录用 perf 行的 overlay_ms） */
  overlay_p95: 150,
  /** 空闲卸载之后重建页面的冷唤出 prepare→shown p95（含原生首帧掩盖手段，perf 行 page=cold） */
  cold_show_p95: 150,
  /** ready 之后原生窗口上屏 / 面板成为 key 的 p95（输入视图 ready→shown 的 Rust 侧拆分） */
  ready_split_p95: 100,
  /**
   * 冒烟结束时主进程 RSS 的宽松绝对上限（不依赖基线，拦住 100 次触发后的明显泄漏）：release 为空闲预算的 2 倍、
   * 超出即失败；debug 构建没有优化、RSS 本身大得多（macOS 实测约 129 MiB），只告警
   */
  smoke_rss_main: {
    release: { macos: 80, windows: 80, linux: 100 },
    debug: { macos: 256, windows: 256, linux: 320 },
  },
}

/**
 * 退化判定的测量噪声下限：本次超过「基线 ×(1 + regression)」**且**超过「基线 + 下限」才算退化（「退化 > 10%」
 * 对接近 0 的指标没有意义：基线 0.01% 的 CPU 多出 0.002 个百分点就是 +20%）。取值对应各指标的
 * 测量分辨率：RSS 0.5 MiB（分配器页对齐的抖动）；CPU 0.01 个百分点（600 s 内 60 ms 的 CPU 时间，ps 的计时单位是
 * 10 ms）；唤醒 0.2 次/s（10 分钟内 120 次）；延迟 2 ms；冷启动 20 ms。基线的 `gate.floor` 可以按实测抖动逐项覆盖
 * （取值已按同机重复测量的实际抖动确认）。
 */
export const FLOOR = { rss: 0.5, cpu: 0.01, wakeups: 0.2, latency: 2, cold_start: 20 }
const DEFAULT_REGRESSION = 0.1
/** 冒烟结束时不应再存在的窗口（设置、引导、固定窗口都应已关闭；panel 与预建的 overlay 常驻） */
const TRANSIENT_WINDOW = /^(?:settings|onboarding|pinned-.*)$/
/** --require-webview：WebView 进程数下限（macOS：GPU / Networking / WebContent；Linux：WebKitWebProcess + NetworkProcess；
 * Windows：WebView2 的浏览器 / GPU / 渲染进程） */
export const MIN_WEBVIEW_PROCESSES = { macos: 3, windows: 3, linux: 2 }
/**
 * Apple 虚拟机（GitHub 的 macos-15 runner，`machine` = VirtualMac2,1）上的 WebKit **没有 GPU 进程**，只有 WebContent 与
 * Networking（measure-macos.sh 的 notes 记录了进程类型）；软件合成下面板第一次唤出 ready 127 ms、上屏 197–472 ms，
 * 撞上 40 ms 的 ready 兜底。虚拟机记录因此：WebView 进程下限 2；启动后第一次唤出的 ready 超时、ready 超时次数与延迟预算
 * 只告警（唤出延迟以真机测量为准）。**内存预算（主进程、回收前 / 回收后两个窗口）照常硬判**
 */
export const MIN_WEBVIEW_PROCESSES_VM = { macos: 2 }
/** 记录是否来自 Apple 虚拟机（record.mjs 写入的 `machine`，sysctl hw.model） */
export const isAppleVm = (r) => r?.os === 'macos' && (r?.machine ?? '').startsWith('VirtualMac')
/**
 * Hyper-V 虚拟机（GitHub 的 Windows / Linux runner：`machine` = "Virtual Machine"——measure-windows.ps1 取
 * Win32_ComputerSystem.Model、measure-linux.sh 取 DMI 的 product_name）同样是共享 vCPU、没有 GPU 的软件合成：面板回收对照
 * （ci.yml 的 panel-recycle 作业）的「用过之后」一段在 runner 上会因 exercise 的 1 次 ready 超时被判无效（剪贴板视图 ready 85 ms，
 * 兜底是 40 ms），而这项对照要的是内存、CPU 与唤醒。唤出延迟按 Apple 虚拟机同样处理：ready 超时（含启动后第一次唤出）
 * 与延迟预算只告警；**内存、CPU、唤醒与测量有效性照常判定**，WebView 进程下限不变
 */
export const isHyperVVm = (r) =>
  (r?.os === 'windows' || r?.os === 'linux') && (r?.machine ?? '').trim() === 'Virtual Machine'
/** 唤出延迟不代表真机的虚拟机记录：返回告警里附带的说明（不是虚拟机时为空串） */
export function latencyVmNote(r) {
  if (isAppleVm(r)) {
    return '（Apple 虚拟机：没有 GPU 进程、软件合成，唤出延迟不代表真机——只告警，以真机测量为准）'
  }
  if (isHyperVVm(r)) {
    return '（Hyper-V 虚拟机（GitHub 的 Windows / Linux runner）：共享 vCPU、没有 GPU，唤出延迟不代表真机——只告警，以真机测量为准）'
  }
  return ''
}
/** --require-webview：各平台可靠的 WebView 归属（macOS 的近似归属可能混入同一终端的其他 WebKit 应用） */
const RELIABLE_ATTRIBUTION = {
  macos: ['responsible'],
  windows: ['descendants'],
  linux: ['descendants'],
}
/** 会话后空闲的状态（record.mjs 由应用日志得出，`idle.session.state`） */
export const SESSION_STATES = {
  hot: '回收前',
  recycled: '回收后',
  unloaded: '卸载后',
  mixed: '采样窗口内发生了回收 / 卸载',
  unknown: '状态未知（读不到应用日志）',
}
/** 系统大版本（"26.4.0" → "26"；Windows 的 "Microsoft Windows 11 … 10.0.26100" 取最后一段的主版本） */
function osMajor(v) {
  if (typeof v !== 'string' || !v.trim()) return null
  const last = v.trim().split(/\s+/).at(-1)
  return last.split('.')[0] || null
}

/**
 * 评估一份报告。返回 `{ failures, warnings, passes, notes }`（每项是一行中文说明）。
 * `opts`：`{ ci, expectTriggers, baselineExpected, requireWebview, expectState, expectViews, webviewGate }`；baselineExpected 表示调用方
 * 给了基线路径但文件不存在（与「没有给基线」区分：CI 的 macOS 冒烟据此告警 RSS 退化门禁没有生效）；后三项只用于性能
 * 记录（见文件头「WebView 内存与会话后空闲」）。
 */
export function evaluate(report, baseline, opts = {}) {
  const ci = opts.ci === true
  const expectTriggers = opts.expectTriggers ?? 100
  const out = { failures: [], warnings: [], passes: [], notes: [] }
  const fail = (msg) => out.failures.push(msg)
  const pass = (msg) => out.passes.push(msg)
  /** 按严重度登记：hard 为真时失败，否则告警 */
  const judge = (hard, okCond, okMsg, badMsg) => {
    if (okCond) pass(okMsg)
    else if (hard) fail(badMsg)
    else out.warnings.push(badMsg)
  }

  const { kind, errors } = validateDocument(report)
  if (errors.length) {
    fail(
      `报告格式不合法（README「冒烟报告」/「性能记录」）：\n    ${errors.slice(0, 20).join('\n    ')}`,
    )
    return out
  }
  if (baseline) {
    const b = validateDocument(baseline)
    if (b.errors.length || b.kind !== 'perf') {
      fail(`基线格式不合法（应为 wisp.perf/1）：\n    ${b.errors.slice(0, 20).join('\n    ')}`)
      return out
    }
    if (baseline.os !== report.os) {
      fail(`基线是 ${baseline.os} 的，报告是 ${report.os} 的`)
      return out
    }
  }
  const os = report.os
  const regression = baseline?.gate?.regression ?? DEFAULT_REGRESSION
  const maxTimeouts = baseline?.gate?.max_ready_timeouts ?? 0
  const floors = { ...FLOOR, ...baseline?.gate?.floor }
  const pct = `${Math.round(regression * 100)}%`

  /** 与基线比较（`metric` 选 FLOOR 中的下限）；基线或本次没有数据时跳过 */
  const compare = (label, now, base, metric, hard, unit) => {
    if (base === null || base === undefined || now === null || now === undefined) {
      out.notes.push(`${label}：基线或本次没有数据，跳过比较`)
      return
    }
    const floor = floors[metric]
    const limit = Math.max(base * (1 + regression), base + floor)
    const delta =
      base > 0 ? `${now >= base ? '+' : ''}${round(((now - base) / base) * 100, 1)}%` : ''
    judge(
      hard,
      now <= limit,
      `${label} ${now}${unit}（基线 ${base}${unit}，${delta}）`,
      `${label} 退化：${now}${unit}，基线 ${base}${unit}（${delta}；上限 +${pct} 且 +${floor}${unit}）`,
    )
  }

  if (kind === 'smoke') {
    evaluateSmoke(report, baseline, { ...opts, ci, expectTriggers, maxTimeouts })
    return out
  }
  evaluateRecord(report, baseline, opts)
  return out

  function evaluateSmoke(r, base, o) {
    const build = smokeBuild(r)
    const release = build === 'release'
    const t = r.triggers
    const s = smokeSummary(r)
    judge(true, r.ok, '冒烟流程完成（ok=true）', '冒烟流程没有完成（ok=false）')
    judge(
      true,
      r.failures.length === 0,
      '没有失败项',
      `冒烟失败项：\n    ${r.failures.join('\n    ')}`,
    )
    judge(
      true,
      r.fake_llm === true,
      '使用 fake LLM',
      '没有使用 fake LLM（需要 WISP_FAKE_LLM=1 与 --features fake-llm 构建）：翻译流程不可复现',
    )
    judge(
      true,
      r.startup.tray_ready_ms !== null && r.panel_prewarm_ms !== null,
      `托盘就绪 ${r.startup.tray_ready_ms} ms、Panel 预热 ${r.panel_prewarm_ms} ms`,
      `启动阶段不完整：托盘就绪 ${r.startup.tray_ready_ms ?? '—'} ms、Panel 预热 ${r.panel_prewarm_ms ?? '—'} ms`,
    )
    const ft = r.first_translate
    judge(
      true,
      ft !== null && ft.finished && ft.shown_ms !== null,
      `首次翻译完成（显示 ${ft?.shown_ms} ms，首 token ${ft?.first_token_ms} ms，全程 ${ft?.elapsed_ms} ms）`,
      '首次 TranslateText 没有完成（面板没有显示，或 fake 流没有结束）',
    )
    // 撞上 ready 超时的那一次，冒烟不等 40 ms 的 ready 兜底上屏就进入下一次（smoke.rs 的 trigger 以 Shown 或 ReadyTimeout 结束），
    // 记为没有显示：它由下面的 ready 超时上限判定（默认 0；runner 基线可放宽），这里只要求「显示或超时」
    // 覆盖每一次——既没有显示也没有超时的是卡死
    const timedOut = t.ready_timeouts ?? 0
    judge(
      true,
      t.count >= o.expectTriggers && t.shown + timedOut >= t.count,
      `连续触发 ${t.count} 次，全部显示${t.shown < t.count ? `（${t.count - t.shown} 次 ready 超时、按上限另判）` : ''}`,
      `连续触发不完整：count=${t.count}（应 ≥ ${o.expectTriggers}），显示 ${t.shown} 次、ready 超时 ${timedOut} 次`,
    )
    judge(true, t.hangs === 0, '0 次卡死', `卡死 ${t.hangs} 次`)
    // 首次翻译的 ready 超时同样计入（旧报告没有这个字段，按 false）
    const firstTimeout = ft?.ready_timeout === true ? 1 : 0
    const timeouts = t.ready_timeouts + firstTimeout
    judge(
      true,
      timeouts <= o.maxTimeouts,
      `ready 超时 ${timeouts} 次（含首次翻译 ${firstTimeout} 次；上限 ${o.maxTimeouts}）`,
      `ready 超时 ${timeouts} 次（含首次翻译 ${firstTimeout} 次），超过上限 ${o.maxTimeouts}（40 ms 的 ready 兜底不应触发）`,
    )
    // 启动后第一次唤出（首次翻译、连续触发的第 1 次）撞上 ready 超时一律失败——它们最容易撞上 40 ms 的 ready 兜底，
    // 不能被基线的 gate.max_ready_timeouts 一并放过
    const firstTriggerTimeout = t.first?.ready_timeout === true ? 1 : 0
    judge(
      true,
      firstTimeout + firstTriggerTimeout === 0,
      '启动后第一次唤出（首次翻译、连续触发第 1 次）没有 ready 超时',
      `启动后第一次唤出撞上 ready 超时（首次翻译 ${firstTimeout} 次、连续触发第 1 次 ${firstTriggerTimeout} 次）：第一次唤出同样要在预算内，不受 gate.max_ready_timeouts 放宽`,
    )
    const sw = r.settings_window
    const w = r.windows
    const leaked = w.labels_end.filter((l) => TRANSIENT_WINDOW.test(l))
    judge(
      true,
      sw.opened && sw.closed && leaked.length === 0 && w.end < w.peak,
      `设置窗口打开 ${sw.open_ms} ms、关闭 ${sw.close_ms} ms，窗口数从峰值 ${w.peak} 回到 ${w.end}（${w.labels_end.join('、')}）`,
      !sw.opened || !sw.closed
        ? `设置窗口没有${sw.opened ? '关闭' : '打开'}`
        : `窗口泄漏：结束时仍有 ${w.end} 个窗口（${w.labels_end.join('、')}，峰值 ${w.peak}），设置关闭后应回收`,
    )

    const p95 = t.prepare_to_shown_ms.p95
    const debugNote = release ? '' : '（debug 构建，仅告警）'
    judge(
      release && !o.ci,
      p95 !== null && p95 <= BUDGET.prepare_to_shown_p95,
      `prepare→shown p95 ${p95} ms（预算 ${BUDGET.prepare_to_shown_p95} ms）`,
      `prepare→shown p95 ${p95 ?? '—'} ms 超出预算 ${BUDGET.prepare_to_shown_p95} ms${debugNote}`,
    )
    // 首次翻译、第 1 次与最后一次触发的「触发 → 显示」都在预算内（release；debug / CI 只告警）
    const ftShown = ft?.shown_ms ?? null
    judge(
      release && !o.ci,
      ftShown !== null && ftShown <= BUDGET.trigger_to_shown_ms,
      `首次翻译 触发→显示 ${ftShown} ms（预算 ${BUDGET.trigger_to_shown_ms} ms）`,
      `首次翻译 触发→显示 ${ftShown ?? '—'} ms 超出预算 ${BUDGET.trigger_to_shown_ms} ms${debugNote}`,
    )
    for (const [name, sample] of [
      ['第 1 次', t.first],
      ['最后一次', t.last],
    ]) {
      if (!sample) continue
      const v = sample.trigger_to_shown_ms
      judge(
        release && !o.ci,
        sample.shown && !sample.ready_timeout && v !== null && v <= BUDGET.trigger_to_shown_ms,
        `连续触发${name}（#${sample.index}）触发→显示 ${v} ms`,
        `连续触发${name}（#${sample.index}）${sample.ready_timeout ? 'ready 超时，' : ''}触发→显示 ${v ?? '—'} ms 超出预算 ${BUDGET.trigger_to_shown_ms} ms${debugNote}`,
      )
    }
    // Overlay 上屏：旧报告没有这一段
    const ov = r.overlay
    if (ov) {
      judge(
        true,
        ov.shown === ov.count,
        `Overlay 采样 ${ov.count} 次全部显示`,
        `Overlay 采样 ${ov.count} 次只显示了 ${ov.shown} 次`,
      )
      judge(
        release && !o.ci,
        ov.p95 !== null && ov.p95 <= BUDGET.overlay_p95,
        `Overlay 上屏 p95 ${ov.p95} ms（预算 ${BUDGET.overlay_p95} ms）`,
        `Overlay 上屏 p95 ${ov.p95 ?? '—'} ms 超出预算 ${BUDGET.overlay_p95} ms${debugNote}`,
      )
    } else {
      out.notes.push('报告没有 overlay 段（旧版本）：跳过 Overlay 上屏的检查')
    }
    const cold = r.startup.tray_ready_ms
    judge(
      release && !o.ci,
      cold !== null && cold <= BUDGET.cold_start_ms,
      `冷启动到托盘就绪 ${cold} ms（预算 ${BUDGET.cold_start_ms} ms）`,
      `冷启动到托盘就绪 ${cold ?? '—'} ms 超出预算 ${BUDGET.cold_start_ms} ms${debugNote}`,
    )
    // 会话之后的空闲（用过面板之后不能留下周期性的工作）：主进程 CPU 与唤醒按空闲预算判定
    const psi = r.post_session_idle
    if (psi) {
      if (psi.cpu_pct !== null) {
        judge(
          release,
          psi.cpu_pct < BUDGET.cpu_avg,
          `会话后空闲 ${psi.seconds} s：主进程 CPU ${psi.cpu_pct}%（预算 < ${BUDGET.cpu_avg}%）`,
          `会话后空闲 ${psi.seconds} s：主进程 CPU ${psi.cpu_pct}% 超出预算 ${BUDGET.cpu_avg}%（隐藏后仍有定时器 / 轮询在跑）${debugNote}`,
        )
      }
      if (psi.wakeups_per_s !== null) {
        const wcap = BUDGET.wakeups_main[os]
        judge(
          release,
          psi.wakeups_per_s <= wcap,
          `会话后空闲：主进程唤醒 ${psi.wakeups_per_s} 次/s（预算 ≤ ${wcap} 次/s）`,
          `会话后空闲：主进程唤醒 ${psi.wakeups_per_s} 次/s 超出预算 ${wcap} 次/s（空闲唤醒预算）${debugNote}`,
        )
      }
    } else {
      out.notes.push('报告没有 post_session_idle（旧版本）：跳过会话后空闲的检查')
    }
    const cap = BUDGET.smoke_rss_main[build][os]
    judge(
      release,
      s.rss_main !== null && s.rss_main <= cap,
      `冒烟结束时主进程 RSS ${s.rss_main} MiB（峰值 ${s.rss_main_peak} MiB，宽松上限 ${cap} MiB）`,
      `冒烟结束时主进程 RSS ${s.rss_main ?? '—'} MiB 超出宽松上限 ${cap} MiB${debugNote}`,
    )
    // run-smoke.mjs 采的 WebView 合计：只记录，不对照 WebView 内存预算（冒烟流程的 WebContent 读数随 GC 时机摆动，
    // 见文件头；WebView 内存由 measure-macos.sh --exercise 的会话后空闲记录判定）
    const wv = r.webview
    if (wv && s.rss_webview !== null) {
      out.notes.push(
        `冒烟期间 WebView 合计（只记录）：会话后 ${s.rss_webview} MiB（${wv.processes} 个进程），全程峰值 ${s.rss_webview_peak} MiB（归属 ${wv.attribution}）`,
      )
    } else if (wv) {
      out.notes.push(
        `冒烟期间没有采到 WebView 进程${wv.notes.length ? `（${wv.notes.join('；')}）` : ''}`,
      )
    } else {
      out.notes.push('报告没有 webview 段（不是 run-smoke.mjs 跑的、平台不支持采样，或旧版本）')
    }

    // 与基线比较：只和同构建类型的冒烟段比（release / debug 的 RSS、耗时不可比）
    const rssHard = !o.ci || os === 'macos'
    if (!base) {
      if (o.baselineExpected && o.ci && os === 'macos') {
        out.warnings.push(
          `没有基线：macOS 冒烟的 RSS 退化门禁没有生效（用 ${build} 构建的冒烟生成 scripts/perf/baselines/macos.json 的 smoke.${build}，见 scripts/perf/README.md「基线」）`,
        )
      } else {
        out.notes.push('没有基线：跳过 RSS / 冷启动 / 延迟的退化比较')
      }
      return
    }
    // 基线没有冒烟段（只有空闲段的基线）与缺少同构建类型的冒烟段同样处理：冒烟 RSS 的硬门禁不能静默失效
    // （只告警的话，CI 的 macOS 冒烟会一直没有真正比较 RSS）
    const bs = base.smoke?.[build]
    if (!bs) {
      judge(
        rssHard,
        false,
        '',
        `基线没有 ${build} 构建的冒烟段（smoke.${build}），无法做 RSS 退化比较：用 record.mjs --update <基线> --smoke <${build} 构建的冒烟报告> 补上（README「基线」）`,
      )
      return
    }
    // 跨系统大版本（本机 macOS 26 录的基线、runner 是 macOS 15）的 phys_footprint 不可比：退化比较只告警，宽松绝对上限照常
    const nowMajor = osMajor(r.os_version)
    const baseMajor = osMajor(bs.os_version)
    const crossOs = nowMajor !== null && baseMajor !== null && nowMajor !== baseMajor
    if (crossOs) {
      out.warnings.push(
        `基线的 ${build} 冒烟段录于系统 ${bs.os_version}，本次是 ${r.os_version}：跨系统大版本的内存不可比，RSS 退化比较只告警——在同一环境上重录（record.mjs --update <基线> --smoke <报告>，README「基线」）`,
      )
    }
    if (sameMemoryMetric(s, bs)) {
      compare(
        `冒烟主进程 RSS（${build}）`,
        s.rss_main,
        bs.rss_main,
        'rss',
        rssHard && !crossOs,
        ' MiB',
      )
    } else {
      out.warnings.push(
        `冒烟内存口径与基线不同（${s.memory_metric} / 基线 ${bs.memory_metric ?? 'rss'}），跳过 RSS 退化比较：按新口径重新生成基线`,
      )
    }
    compare(`冷启动（${build}）`, s.cold_start_ms, bs.cold_start_ms, 'cold_start', false, ' ms')
    compare(`prepare→shown p95（${build}）`, p95, bs.prepare_to_shown_p95, 'latency', !o.ci, ' ms')
  }

  /** 性能记录的 latency 段（日志或冒烟来源）：预算与 ready 超时；分页、拆分、首次唤出与 Overlay */
  function evaluateLatency(lat, baseLatency, vmNote = '') {
    const vm = vmNote !== ''
    const hard = !ci && !vm
    if (lat.count > 0) {
      const timeouts = lat.ready_timeouts ?? 0
      judge(
        !vm,
        timeouts <= maxTimeouts,
        `ready 超时 ${timeouts} 次`,
        `ready 超时 ${timeouts} 次，超过上限 ${maxTimeouts}${vmNote}`,
      )
      judge(
        hard,
        lat.p95 !== null && lat.p95 <= BUDGET.prepare_to_shown_p95,
        `prepare→shown p95 ${lat.p95} ms（预算 ${BUDGET.prepare_to_shown_p95} ms${lat.pages?.cold ? '，不含冷唤出' : ''}）`,
        `prepare→shown p95 ${lat.p95} ms 超出预算 ${BUDGET.prepare_to_shown_p95} ms`,
      )
      if (baseLatency) {
        compare('prepare→shown p95', lat.p95, baseLatency.p95, 'latency', hard, ' ms')
      }
    }
    // 每次启动后的第一次唤出——ready 超时一律失败（不受 max_ready_timeouts 放宽），耗时按 100 ms 预算
    const fs = lat.first_show
    if (fs && fs.count > 0) {
      judge(
        !vm,
        fs.ready_timeouts === 0,
        `启动后第一次唤出 ${fs.count} 次，没有 ready 超时`,
        `启动后第一次唤出 ${fs.count} 次里 ${fs.ready_timeouts} 次 ready 超时（不受 gate.max_ready_timeouts 放宽）${vmNote}`,
      )
      // 没有显示只告警：真实使用的日志里第一次触发可能因 NotReady（没有服务商、权限缺失……）等正当原因不显示面板；
      // 冒烟的首次唤出由 first_translate / triggers.first 硬判，会话后测量的 exercise 由 --expect-state 硬判
      if ((fs.not_shown ?? 0) > 0) {
        out.warnings.push(
          `启动后第一次唤出 ${fs.count} 次里 ${fs.not_shown} 次没有显示面板（NotReady 之类的正当原因，或 ready 之后没有上屏——对照日志确认）`,
        )
      }
      judge(
        hard,
        fs.max !== null && fs.max <= BUDGET.prepare_to_shown_p95,
        `启动后第一次唤出 prepare→shown 最长 ${fs.max} ms（预算 ${BUDGET.prepare_to_shown_p95} ms）`,
        `启动后第一次唤出 prepare→shown 最长 ${fs.max ?? '—'} ms 超出预算 ${BUDGET.prepare_to_shown_p95} ms`,
      )
    }
    // 冷唤出（空闲卸载之后重建页面）。卡片上屏（prepare→window_ms，原生外壳）是硬门禁；内容首帧（prepare→shown）
    // 超过 150 ms 只告警——它决定的是空闲卸载能不能默认开启（达不到就默认关闭），不是这次运行的对错。
    // 没有 window 分项的旧记录仍按内容首帧判
    const cold = lat.pages?.cold
    if (cold && cold.count > 0) {
      const card = cold.window
      if (card && card.count > 0) {
        judge(
          hard,
          card.p95 !== null && card.p95 <= BUDGET.cold_show_p95,
          `冷唤出（page=cold）${card.count} 次卡片上屏 p95 ${card.p95} ms（预算 ${BUDGET.cold_show_p95} ms）`,
          `冷唤出（page=cold）${card.count} 次卡片上屏 p95 ${card.p95 ?? '—'} ms 超出预算 ${BUDGET.cold_show_p95} ms`,
        )
        if (cold.p95 !== null && cold.p95 > BUDGET.cold_show_p95) {
          out.warnings.push(
            `冷唤出内容首帧 p95 ${cold.p95} ms 超过 ${BUDGET.cold_show_p95} ms：空闲卸载不能默认开启`,
          )
        } else {
          out.notes.push(
            `冷唤出内容首帧 p95 ${cold.p95 ?? '—'} ms（预算 ${BUDGET.cold_show_p95} ms）`,
          )
        }
      } else {
        judge(
          hard,
          cold.p95 !== null && cold.p95 <= BUDGET.cold_show_p95,
          `冷唤出（page=cold）${cold.count} 次 prepare→shown p95 ${cold.p95} ms（预算 ${BUDGET.cold_show_p95} ms）`,
          `冷唤出（page=cold）${cold.count} 次 prepare→shown p95 ${cold.p95 ?? '—'} ms 超出预算 ${BUDGET.cold_show_p95} ms（达不到时空闲卸载默认关闭）`,
        )
      }
    }
    const recycled = lat.pages?.recycled
    if (recycled && recycled.count > 0) {
      out.notes.push(
        `回收出来的新页面（page=recycled）${recycled.count} 次 prepare→shown p95 ${recycled.p95} ms（已计入主指标）`,
      )
    }
    // 40.9：ready 之后的 Rust 侧拆分（原生窗口上屏 / 成为 key）
    for (const [key, label] of [
      ['ready_to_window', 'ready→原生窗口上屏'],
      ['ready_to_key', 'ready→成为 key'],
    ]) {
      const sp = lat.split?.[key]
      if (!sp || sp.count === 0) continue
      judge(
        hard,
        sp.p95 !== null && sp.p95 <= BUDGET.ready_split_p95,
        `${label} p95 ${sp.p95} ms（${sp.count} 次，预算 ${BUDGET.ready_split_p95} ms）`,
        `${label} p95 ${sp.p95 ?? '—'} ms 超出预算 ${BUDGET.ready_split_p95} ms（${sp.count} 次）`,
      )
    }
    // Overlay 上屏（perf 行的 overlay_ms）
    if ((lat.overlay_count ?? 0) > 0) {
      judge(
        hard,
        lat.overlay_p95 !== null && lat.overlay_p95 <= BUDGET.overlay_p95,
        `Overlay 上屏 ${lat.overlay_count} 次 p95 ${lat.overlay_p95} ms（预算 ${BUDGET.overlay_p95} ms）`,
        `Overlay 上屏 ${lat.overlay_count} 次 p95 ${lat.overlay_p95 ?? '—'} ms 超出预算 ${BUDGET.overlay_p95} ms`,
      )
    }
  }

  function evaluateRecord(r, base, o = {}) {
    // wisp.perf/1：空闲测量 + 延迟 + 冒烟摘要
    const hard = !ci
    // 空闲段只和同构建类型的基线比较（debug 记录拿去和 release 基线比，主进程 RSS 会报 +55% 以上，结论没有意义）
    if (base && r.build && base.build && r.build !== base.build) {
      out.warnings.push(
        `记录是 ${r.build} 构建、基线是 ${base.build} 构建：跳过空闲 / 延迟的退化比较（只检查预算）；用同构建类型的基线比较`,
      )
      base = { ...base, idle: undefined, latency: undefined }
    }
    const idle = r.idle
    const gateHard = o.webviewGate === 'hard'
    const requireWebview = o.requireWebview === true || gateHard
    if ((requireWebview || o.expectState) && !idle) {
      fail('记录没有 idle 段：--require-webview / --expect-state 需要一次空闲测量')
    }
    if (idle) {
      const session = idle.session ?? null
      const stateLabel = session ? (SESSION_STATES[session.state] ?? session.state) : null
      const at = stateLabel ? `（${stateLabel}）` : ''
      // 主进程内存：CI 上 release 构建且 WebView 精确归属（经 LaunchServices 启动）时同样是硬门禁（不受 runner 时间抖动影响）
      const viaLaunchServices =
        os === 'macos' && r.build === 'release' && idle.webview_attribution === 'responsible'
      const memHard = hard || viaLaunchServices
      judge(
        memHard,
        idle.rss_main <= BUDGET.rss_main[os],
        `空闲主进程 RSS ${idle.rss_main} MiB${at}（预算 ${BUDGET.rss_main[os]} MiB）`,
        `空闲主进程 RSS ${idle.rss_main} MiB${at} 超出预算 ${BUDGET.rss_main[os]} MiB`,
      )
      // 启动环境：经 LaunchServices 启动的 macOS release 记录，Info.plist 的 LSEnvironment
      // （MallocSpaceEfficient=1）必须生效——打包丢了这一段时本机主进程从约 13 MiB 回到约 21 MiB，而 runner（macOS 15）上的
      // 内存读数未必看得出来，所以直接核对应用日志里的 launch_env（record.mjs 写进 idle.launch_env）。读不到这一项时，
      // --require-webview（CI 的两个会话后窗口、正式测量）失败，否则告警
      if (viaLaunchServices) {
        if (idle.launch_env === true) {
          pass(
            '启动环境生效（launch_env=true：Info.plist 的 LSEnvironment，MallocSpaceEfficient=1）',
          )
        } else if (idle.launch_env === false) {
          fail(
            '经 LaunchServices 启动，启动环境却没有生效（launch_env=false）：打出来的 .app 的 Info.plist 没有带上 LSEnvironment（MallocSpaceEfficient=1）？主进程会从约 13 MiB 回到约 21 MiB',
          )
        } else {
          judge(
            requireWebview,
            false,
            '',
            '记录里没有 launch_env（读不到应用日志，或应用版本较旧、启动日志没有这一项）：无法核对 MallocSpaceEfficient 是否生效',
          )
        }
      }
      // 测量本身有效（--require-webview）：WebView 必须测到、归属可靠、进程数不少于下限——否则内存预算与空闲 CPU 只验证了
      // 主进程，WebView 内存门禁会悄悄失效
      if (requireWebview) {
        const reliable = RELIABLE_ATTRIBUTION[os] ?? []
        const vm = isAppleVm(r)
        const minProcs =
          (vm ? MIN_WEBVIEW_PROCESSES_VM[os] : undefined) ?? MIN_WEBVIEW_PROCESSES[os] ?? 1
        const problems = []
        if (idle.rss_webview === null) problems.push('WebView 进程没有测到')
        if (!reliable.includes(idle.webview_attribution)) {
          problems.push(
            `归属是 ${idle.webview_attribution ?? '—'}（应为 ${reliable.join(' / ')}${os === 'macos' ? '：用 measure-macos.sh --app <Wisp.app> 经 LaunchServices 启动' : ''}）`,
          )
        }
        if (!(idle.webview_processes >= minProcs)) {
          problems.push(`WebView 进程 ${idle.webview_processes ?? 0} 个（下限 ${minProcs}）`)
        }
        if (os === 'macos' && r.build !== 'release') {
          problems.push(`构建类型是 ${r.build ?? '—'}（应为 release）`)
        }
        judge(
          true,
          problems.length === 0,
          `WebView 测量有效（${idle.webview_processes} 个进程，归属 ${idle.webview_attribution}${vm ? '；Apple 虚拟机没有 GPU 进程，下限 2' : ''}）`,
          `WebView 测量无效（--require-webview）：${problems.join('；')}`,
        )
      }
      // 会话状态（--expect-state）：exercise 确实唤出过面板，采样窗口的状态与预期一致
      if (o.expectState) {
        const want = SESSION_STATES[o.expectState] ?? o.expectState
        if (!session) {
          fail(
            `记录没有会话信息（idle.session）：--expect-state ${o.expectState} 需要 measure-*.sh --exercise 并能读到应用日志`,
          )
        } else if (session.shown === null) {
          fail(
            `读不到应用日志（${session.log ?? '—'}）：无法核对 exercise 是否唤出了面板、采样窗口是不是「${want}」`,
          )
        } else {
          judge(
            true,
            session.shown >= 1,
            `exercise 唤出面板 ${session.shown} 次（日志里显示过的 perf 追踪）`,
            `exercise 没有唤出面板（日志里 exercise 之后没有显示过的 perf 追踪，${session.traces ?? 0} 条追踪）：测到的是「从未显示」的状态，--trigger 转发失败？`,
          )
          // exercise 的最后状态：采样窗口里面板必须已隐藏（否则「回收前」测的是可见的面板、「回收后」因为没有回收而误报）
          const panel = session.panel ?? 'unknown'
          const views = (session.views ?? []).join('、') || '—'
          const hides =
            Object.entries(session.hides ?? {})
              .map(([k, n]) => `${k} ×${n}`)
              .join('、') || '—'
          judge(
            true,
            panel === 'hidden',
            `采样窗口里面板隐藏着（唤出 ${views}；隐藏 ${hides}；最后一次隐藏在 exercise 结束后 ${session.last_hide_s ?? '—'} s）`,
            panel === 'visible'
              ? `采样窗口里面板还在屏幕上（唤出 ${views}；隐藏 ${hides}）：exercise 没有把面板关掉——失焦先隐藏后再次 --trigger 会重新打开面板；用 scripts/perf/exercise.mjs（每一步由日志核对）后重跑`
              : `无法核对采样窗口里面板是否已隐藏：应用日志里没有 debug 级别的行（advanced.log_level 不是 debug；measure-*.sh 在隔离 HOME 里用 --settings preset 打开，--home keep / Windows 的真实目录要自己在设置里打开）`,
          )
          const events = [
            ...(session.recycles_s ?? []).map((t) => `${t} s 回收`),
            ...(session.unloads_s ?? []).map((t) => `${t} s 卸载`),
          ]
          judge(
            true,
            session.state === o.expectState,
            `采样窗口是「${want}」（exercise 之后 ${session.window_s.join('–')} s）`,
            `采样窗口应是「${want}」，实际是「${stateLabel}」：exercise 之后 ${session.window_s.join('–')} s 采样，${events.length ? events.join('、') : '期间没有回收 / 卸载'}——面板回收时长（windows::manager::panel_recycle_after，macOS 45 s）或卸载设置变了？调整 --settle / --idle 或 CI 的采样窗口`,
          )
        }
      }
      // 用到了哪些视图（--expect-views）：不规定次数，规定每个视图至少上屏过一次
      if (o.expectViews?.length) {
        const seen = session?.views ?? null
        const missing = seen ? o.expectViews.filter((v) => !seen.includes(v)) : o.expectViews
        judge(
          true,
          seen !== null && missing.length === 0,
          `exercise 用到了 ${o.expectViews.join('、')}（上屏 ${seen?.join('、')}）`,
          seen === null
            ? `--expect-views：记录里没有上屏视图的信息（没有会话信息或应用日志里没有 debug 行）`
            : `exercise 没有用到 ${missing.join('、')}（上屏的视图：${seen.join('、') || '—'}）`,
        )
      }
      // WebView 没有测到时不能当作 0：预算与空闲 CPU 都只验证了主进程的一半
      const why = idle.notes.length ? `（${idle.notes.join('；')}）` : ''
      // --ci 下 WebView 预算只在 --webview-gate hard 时是硬门禁（CI 的两个窗口都传 hard，见 ci.yml）。macOS 的回收前
      // 窗口按防退化上限 rss_webview_hot 判，55 MiB 判回收后
      const wvHard = hard || gateHard
      const hotWindow = os === 'macos' && session?.state === 'hot'
      const wvBudget = hotWindow ? BUDGET.rss_webview_hot.macos : BUDGET.rss_webview[os]
      const wvWhat = hotWindow ? '回收前防退化上限' : '预算'
      const wvTail =
        os === 'macos'
          ? `（macOS WebView 内存${hotWindow ? '；55 MiB 判回收后，回收前按防退化上限' : ''}${gateHard || hard ? '' : '；--ci 下没有 --webview-gate hard 时只告警'}）`
          : ''
      if (idle.rss_webview === null) {
        if (!requireWebview) {
          judge(
            hard,
            false,
            '',
            `WebView 进程没有测到${why}：WebView RSS ≤ ${BUDGET.rss_webview[os]} MiB 与空闲 CPU 只验证了主进程`,
          )
        }
      } else {
        judge(
          wvHard,
          idle.rss_webview <= wvBudget,
          `空闲 WebView RSS ${idle.rss_webview} MiB${at}（${idle.webview_processes} 个进程，${wvWhat} ${wvBudget} MiB）`,
          `空闲 WebView RSS ${idle.rss_webview} MiB${at} 超出${wvWhat} ${wvBudget} MiB${wvTail}`,
        )
        if (idle.cpu_webview_avg === null) {
          judge(hard, false, '', `WebView 进程的 CPU 没有测到${why}：空闲 CPU 只验证了主进程`)
        }
      }
      if (idle.webview_attribution === 'responsible-approx') {
        out.warnings.push(
          'WebView 进程的归属是近似的（Wisp 从终端 / IDE 启动，可能混入同一终端的其他 WebKit 应用）：正式基线请用 measure-macos.sh --app <Wisp.app> 经 LaunchServices 启动',
        )
      }
      // 采样窗口里有人在用（record.mjs 的 idle.activity：复制、或不能用 exercise 解释的 4 Hz 活跃档）：CPU / 唤醒高于「用户
      // 不在」时录的基线是正常的——预算与退化比较都降为告警，说明原因（否则开发机上的测量会被误判成 +1000% 以上的退化）
      const activity = idle.activity ?? null
      const busy = activity !== null && (activity.copies > 0 || (activity.user_active_s ?? 0) > 0)
      const cpuHard = hard && !busy
      if (busy) {
        out.warnings.push(
          `采样窗口里有人在用（${activity.tiers_s ? `剪贴板轮询活跃档 ${activity.user_active_s} s，` : ''}复制 ${activity.copies} 次）：空闲 CPU / 唤醒的预算与基线比较只告警——在安静的机器上重测`,
        )
      } else if (activity === null && hard) {
        out.notes.push(
          '记录里没有用户活动信息（idle.activity：旧记录或日志不是 debug 级别），无法区分「有人在用」与退化',
        )
      }
      if (idle.cpu_total_avg !== null) {
        judge(
          cpuHard,
          idle.cpu_total_avg < BUDGET.cpu_avg,
          `空闲 CPU 合计 ${idle.cpu_total_avg}%（主进程 ${idle.cpu_avg}% + WebView ${idle.cpu_webview_avg}%，预算 < ${BUDGET.cpu_avg}%）`,
          `空闲 CPU 合计 ${idle.cpu_total_avg}% 超出预算 ${BUDGET.cpu_avg}%（主进程 ${idle.cpu_avg}% + WebView ${idle.cpu_webview_avg}%；WebView 偏高通常是隐藏后没有停掉的 rAF / 定时器 / CSS 动画）`,
        )
      } else {
        judge(
          cpuHard,
          idle.cpu_avg < BUDGET.cpu_avg,
          `空闲主进程 CPU ${idle.cpu_avg}%（预算 < ${BUDGET.cpu_avg}%）`,
          `空闲主进程 CPU ${idle.cpu_avg}% 超出预算 ${BUDGET.cpu_avg}%`,
        )
      }
      // 主进程唤醒的绝对预算（不依赖基线；基线为 0 时退化比较只剩噪声下限，这里兜底）
      if (idle.wakeups !== null && idle.wakeups !== undefined) {
        const cap = BUDGET.wakeups_main[os]
        judge(
          cpuHard,
          idle.wakeups <= cap,
          `空闲主进程唤醒 ${idle.wakeups} 次/s（预算 ≤ ${cap} 次/s）`,
          `空闲主进程唤醒 ${idle.wakeups} 次/s 超出预算 ${cap} 次/s（${os === 'macos' ? '唯一的周期唤醒应是 4 Hz 的剪贴板 changeCount 轮询' : 'Windows / X11 空闲时不应有周期唤醒'}；找出新增的定时器 / 轮询）`,
        )
      } else {
        out.notes.push('主进程唤醒没有测到：跳过主进程唤醒预算')
      }
      const bi = base?.idle
      // 基线（空闲段与 gate.session_rss_main）录于哪个系统大版本：跨大版本的 phys_footprint 不可比，主进程的退化比较只告警
      const baseMajor = osMajor(base?.os_version)
      const nowMajor = osMajor(r.os_version)
      const crossOs = baseMajor !== null && nowMajor !== null && baseMajor !== nowMajor
      const crossNote = crossOs
        ? `（基线录于系统 ${base.os_version}，本次是 ${r.os_version}：跨系统大版本的内存不可比，只告警——在同一环境上重录基线，见 scripts/perf/README.md「基线」）`
        : ''
      if (session && base) {
        // 会话后窗口（回收前 / 回收后）：基线的空闲段通常是「从未显示」（runner 基线是会话后的回收后窗口、带 idle.session，
        // 与回收前也不同），WebView / CPU / 唤醒的口径不同，不比较；主进程按基线的 gate.session_rss_main（同一环境实测的会话后
        // 主进程）判退化——经 LaunchServices 启动的 macOS release 记录是硬门禁（分配器设置失效、主进程内存增长在 CI 上
        // 都要拦得住）
        out.notes.push(
          '会话后窗口：不与基线的空闲段比较 WebView / CPU / 唤醒（口径不同：基线的空闲段是从未显示，或是另一种状态的会话后窗口）',
        )
        const ref = base.gate?.session_rss_main
        if (ref === undefined) {
          judge(
            false,
            false,
            '',
            '基线没有 gate.session_rss_main：会话后窗口的主进程只按绝对预算判（见 scripts/perf/README.md「基线」）',
          )
        } else if (!sameMemoryMetric(idle, bi ?? {})) {
          out.warnings.push(
            `会话后主进程：内存口径与基线不同（${idle.memory_metric ?? 'rss'} / 基线 ${bi?.memory_metric ?? 'rss'}），跳过比较`,
          )
        } else {
          compare(`会话后主进程 RSS${at}`, idle.rss_main, ref, 'rss', memHard && !crossOs, ' MiB')
          if (crossNote) out.warnings.push(`会话后主进程 RSS${crossNote}`)
        }
      } else if (bi) {
        // 基线的空闲段本身是会话后窗口（带 idle.session：runner 没有「从未显示」的窗口，runner 基线取的是 ② 回收后）：
        // 与这份从未显示的记录口径不同——WebView / CPU / 唤醒不比较（用过之后 WebKit 的工作集与进程构成都不同，
        // 比较的结论没有意义）；主进程照常比较：同一环境里从未显示的主进程不高于用过之后的（本机实测 13.2–13.3 /
        // 15.1–15.4 MiB），按会话后的读数判是偏宽的上限，超出就是退化
        const baseSession = bi.session ?? null
        if (baseSession) {
          out.notes.push(
            `基线的空闲段是会话后窗口（${SESSION_STATES[baseSession.state] ?? baseSession.state}，idle.session），不是「从未显示」：只比较主进程内存（偏宽的上限），不比较 WebView / CPU / 唤醒（口径不同）`,
          )
        }
        if (sameMemoryMetric(idle, bi)) {
          // 主进程：经 LaunchServices 启动的 macOS release 记录在 CI 上同样硬判（不受 runner 时间抖动影响）
          compare('空闲主进程 RSS', idle.rss_main, bi.rss_main, 'rss', memHard && !crossOs, ' MiB')
          if (crossNote) out.warnings.push(`空闲主进程 RSS${crossNote}`)
          if (!baseSession) {
            compare('空闲 WebView RSS', idle.rss_webview, bi.rss_webview, 'rss', hard, ' MiB')
          }
        } else {
          out.warnings.push(
            `空闲内存口径与基线不同（${idle.memory_metric ?? 'rss'} / 基线 ${bi.memory_metric ?? 'rss'}），跳过内存退化比较：按新口径重新生成基线`,
          )
        }
        if (!baseSession) {
          compare('空闲主进程 CPU', idle.cpu_avg, bi.cpu_avg, 'cpu', cpuHard, '%')
          compare('空闲 WebView CPU', idle.cpu_webview_avg, bi.cpu_webview_avg, 'cpu', cpuHard, '%')
          if (
            idle.wakeups_method &&
            bi.wakeups_method &&
            idle.wakeups_method !== bi.wakeups_method
          ) {
            out.notes.push(
              `唤醒次数的测量方法不同（${idle.wakeups_method} / 基线 ${bi.wakeups_method}），跳过比较`,
            )
          } else {
            compare('空闲主进程唤醒', idle.wakeups, bi.wakeups, 'wakeups', cpuHard, ' 次/s')
            compare(
              '空闲 WebView 唤醒',
              idle.wakeups_webview,
              bi.wakeups_webview,
              'wakeups',
              cpuHard,
              ' 次/s',
            )
          }
        }
      } else if (base) out.notes.push('基线没有 idle 段：跳过空闲指标的退化比较')
    }
    const lat = r.latency
    if (lat) evaluateLatency(lat, base?.latency, latencyVmNote(r))
    const hasLatency = lat && (lat.count > 0 || (lat.overlay_count ?? 0) > 0)
    if (!idle && !hasLatency && !r.smoke) fail('性能记录里没有任何测量数据')
    for (const build of BUILDS) {
      const now = r.smoke?.[build]
      const was = base?.smoke?.[build]
      if (!now || !was) continue
      compare(`冒烟主进程 RSS（${build}）`, now.rss_main, was.rss_main, 'rss', hard, ' MiB')
      compare(
        `冒烟冷启动（${build}）`,
        now.cold_start_ms,
        was.cold_start_ms,
        'cold_start',
        false,
        ' ms',
      )
    }
    if (!base) out.notes.push('没有基线：只检查预算，跳过退化比较')
  }
}

function readJsonFile(path, what) {
  try {
    return JSON.parse(readFileSync(path, 'utf8'))
  } catch (e) {
    throw new Error(`读取${what} ${path} 失败：${e instanceof Error ? e.message : e}`, { cause: e })
  }
}

function printResult(result, title) {
  for (const p of result.passes) ok(p)
  for (const n of result.notes) info(`  · ${n}`)
  for (const w of result.warnings) warn(w)
  for (const f of result.failures) error(f)
  const lines = [`### ${title}`, '']
  for (const f of result.failures) lines.push(`- ❌ ${f.replaceAll('\n', ' ')}`)
  for (const w of result.warnings) lines.push(`- ⚠️ ${w.replaceAll('\n', ' ')}`)
  for (const p of result.passes) lines.push(`- ✅ ${p}`)
  for (const n of result.notes) lines.push(`- ℹ️ ${n}`)
  appendSummary(lines.join('\n'))
}

// ---------------------------------------------------------------------------
// 自检

function example(name) {
  return JSON.parse(readFileSync(new URL(`./examples/${name}`, import.meta.url), 'utf8'))
}

/** 合格的 release 冒烟报告（examples/smoke-report.example.json），按路径覆盖字段 */
function sampleSmoke(patch = {}) {
  const r = example('smoke-report.example.json')
  for (const [path, value] of Object.entries(patch)) {
    const keys = path.split('.')
    let o = r
    for (const k of keys.slice(0, -1)) o = o[k]
    o[keys.at(-1)] = value
  }
  return r
}

function sampleBaseline(over = {}) {
  return {
    schema: 'wisp.perf/1',
    ts: '2026-10-01T08:00:00.000Z',
    os: 'macos',
    version: '0.1.0',
    build: 'release',
    idle: {
      duration_s: 600,
      samples: 600,
      memory_metric: 'phys_footprint',
      rss_main: 34,
      rss_webview: 36,
      webview_attribution: 'responsible',
      cpu_avg: 0.03,
      cpu_webview_avg: 0.01,
      cpu_total_avg: 0.04,
      wakeups: 4.2,
      wakeups_webview: 0.3,
      wakeups_method: 'top-idlew',
      launch_env: true,
      notes: [],
    },
    latency: { source: 'smoke', count: 100, p50: 16, p95: 23 },
    smoke: {
      release: {
        cold_start_ms: 400,
        rss_main: 36,
        prepare_to_shown_p95: 23,
        memory_metric: 'phys_footprint',
        rss_webview: 50,
      },
      debug: {
        cold_start_ms: 150,
        rss_main: 125,
        prepare_to_shown_p95: 35,
        memory_metric: 'phys_footprint',
      },
    },
    ...over,
  }
}

/** 基于基线的空闲记录，覆盖 idle 的字段 */
function sampleIdle(idle) {
  const b = sampleBaseline()
  return { ...b, idle: { ...b.idle, ...idle } }
}

function selfTest() {
  // 示例文件必须符合 schema（README 引用它们作为格式样例）
  for (const name of [
    'smoke-report.example.json',
    'smoke-report.failed-run.json',
    'perf-record.example.json',
    'perf-post-hot.macos-15.json',
    'perf-post-recycled.macos-15.json',
  ]) {
    assert.deepEqual(validateDocument(example(name)).errors, [], name)
  }
  assert.deepEqual(validateDocument(sampleBaseline()).errors, [], 'sampleBaseline')

  const good = evaluate(sampleSmoke(), sampleBaseline(), { ci: true })
  assert.deepEqual(good.failures, [], '正常报告应通过')
  assert.deepEqual(good.warnings, [])
  assert.ok(
    good.passes.some((p) => p.includes('冒烟主进程 RSS（release）')),
    '应与 release 基线比较',
  )

  // 一次真实的失败冒烟输出（TranslateText 没有被处理：没有追踪、100 次卡死）：格式合法，门禁按失败项拦下
  const failedRun = example('smoke-report.failed-run.json')
  const failedResult = evaluate(failedRun, null)
  assert.ok(failedResult.failures.some((f) => f.includes('ok=false')))
  assert.ok(failedResult.failures.some((f) => f.includes('卡死 100 次')))
  assert.ok(failedResult.failures.some((f) => f.includes('首次 TranslateText')))
  assert.equal(latencyFromSmoke(failedRun).p95, null)
  assert.equal(smokeSummary(failedRun).rss_main, 128.8)
  assert.equal(smokeBuild(failedRun), 'debug')

  const cases = [
    ['ok=false', sampleSmoke({ ok: false }), 'fail'],
    ['有失败项', sampleSmoke({ failures: ['settings window did not open'] }), 'fail'],
    ['没有 fake LLM', sampleSmoke({ fake_llm: false }), 'fail'],
    ['没有预热', sampleSmoke({ panel_prewarm_ms: null }), 'fail'],
    ['首次翻译没结束', sampleSmoke({ 'first_translate.finished': false }), 'fail'],
    ['触发不足', sampleSmoke({ 'triggers.count': 99, 'triggers.shown': 99 }), 'fail'],
    ['有触发没显示', sampleSmoke({ 'triggers.shown': 98 }), 'fail'],
    ['卡死', sampleSmoke({ 'triggers.hangs': 1 }), 'fail'],
    ['ready 超时', sampleSmoke({ 'triggers.ready_timeouts': 1 }), 'fail'],
    // 首次翻译的 ready 超时同样计入
    ['首次翻译 ready 超时', sampleSmoke({ 'first_translate.ready_timeout': true }), 'fail'],
    ['Overlay 有采样没显示', sampleSmoke({ 'overlay.shown': 4 }), 'fail'],
    ['Overlay 上屏超预算（CI 只告警）', sampleSmoke({ 'overlay.p95': 190 }), 'warn'],
    ['首次翻译超出唤出预算（CI 只告警）', sampleSmoke({ 'first_translate.shown_ms': 240 }), 'warn'],
    ['设置窗口没关', sampleSmoke({ 'settings_window.closed': false }), 'fail'],
    [
      '窗口泄漏',
      sampleSmoke({ 'windows.end': 2, 'windows.labels_end': ['panel', 'settings'] }),
      'fail',
    ],
    ['macOS RSS 退化（CI 硬门禁）', sampleSmoke({ 'rss_kb.end': 41 * 1024 }), 'fail'],
    ['release RSS 超宽松上限（无基线也失败）', sampleSmoke({ 'rss_kb.end': 90 * 1024 }), 'fail'],
    ['冷启动退化（只告警）', sampleSmoke({ 'startup.tray_ready_ms': 700 }), 'warn'],
    [
      '延迟 p95 超预算（CI 只告警）',
      sampleSmoke({ 'triggers.prepare_to_shown_ms': { p50: 120, p95: 150, max: 160 } }),
      'warn',
    ],
    [
      '会话后空闲唤醒超预算（release 失败）',
      sampleSmoke({ post_session_idle: { seconds: 10, cpu_pct: 0.01, wakeups_per_s: 9 } }),
      'fail',
    ],
    [
      '会话后空闲 CPU 超预算（release 失败）',
      sampleSmoke({ post_session_idle: { seconds: 10, cpu_pct: 0.4, wakeups_per_s: 0.5 } }),
      'fail',
    ],
    [
      '内存口径与基线不同（跳过 RSS 退化比较，只告警）',
      sampleSmoke({ memory_metric: 'rss', 'rss_kb.end': 41 * 1024 }),
      'warn',
    ],
    ['格式错误', sampleSmoke({ 'rss_kb.end': '36' }), 'fail'],
    ['格式错误（旧的 wisp.smoke/1）', { ...sampleSmoke(), schema: 'wisp.smoke/1' }, 'fail'],
  ]
  for (const [name, rep, expect] of cases) {
    const r = evaluate(rep, sampleBaseline(), { ci: true })
    if (expect === 'fail') assert.ok(r.failures.length > 0, `${name}：应失败`)
    else {
      assert.deepEqual(r.failures, [], `${name}：不应失败`)
      assert.ok(r.warnings.length > 0, `${name}：应告警`)
    }
  }
  // 本机门禁（非 CI）下 release 的唤出预算是硬门禁：首次翻译、第 1 次触发超出 100 ms 都失败
  const slowFirst = evaluate(sampleSmoke({ 'first_translate.shown_ms': 240 }), sampleBaseline())
  assert.ok(
    slowFirst.failures.some((f) => f.includes('首次翻译')),
    '首次翻译超出预算应失败',
  )
  const slowTrigger = evaluate(
    sampleSmoke({ 'triggers.first.trigger_to_shown_ms': 180 }),
    sampleBaseline(),
  )
  assert.ok(
    slowTrigger.failures.some((f) => f.includes('第 1 次')),
    '第 1 次触发超出预算应失败',
  )
  // 空闲记录与基线构建类型不同时跳过退化比较（只告警），不拿 debug 去比 release
  const rec = example('perf-record.example.json')
  const crossBuild = evaluate(
    { ...rec, build: rec.build === 'debug' ? 'release' : 'debug' },
    { ...rec, os: rec.os },
  )
  assert.ok(
    crossBuild.warnings.some((w) => w.includes('构建')),
    '构建类型不同应告警',
  )
  // release 的宽松上限不依赖基线
  assert.ok(evaluate(sampleSmoke({ 'rss_kb.end': 90 * 1024 }), null, { ci: true }).failures.length)
  // 基线只有空闲段（smoke: null）与缺少同构建类型的冒烟段同样处理——CI 的 macOS 失败，Linux 只告警
  const idleOnly = evaluate(sampleSmoke(), sampleBaseline({ smoke: null }), { ci: true })
  assert.ok(
    idleOnly.failures.some((f) => f.includes('smoke.release')),
    '基线没有冒烟段时 CI 的 macOS 应失败',
  )
  const idleOnlyLinux = evaluate(
    sampleSmoke({
      os: 'linux',
      memory_metric: 'rss',
      webview: null,
      post_session_idle: { seconds: 10, cpu_pct: 0.01, wakeups_per_s: null },
    }),
    sampleBaseline({ os: 'linux', smoke: null }),
    { ci: true },
  )
  assert.deepEqual(idleOnlyLinux.failures, [], 'Linux 基线没有冒烟段时只告警')
  assert.ok(idleOnlyLinux.warnings.some((w) => w.includes('smoke.release')))

  // 冒烟的 WebView 合计只记录（不判定），采样失败 / 旧报告同样只记说明
  const hugeWv = evaluate(
    sampleSmoke({ webview: { ...sampleSmoke().webview, hot_kb: 300 * 1024 } }),
    sampleBaseline(),
    { ci: true },
  )
  assert.deepEqual(hugeWv.failures, [], '冒烟的 WebView 合计不判定')
  assert.deepEqual(hugeWv.warnings, [])
  assert.ok(hugeWv.notes.some((n) => n.includes('WebView 合计（只记录）：会话后 300 MiB')))
  const oldReport = sampleSmoke()
  delete oldReport.webview
  const oldResult = evaluate(oldReport, sampleBaseline(), { ci: true })
  assert.deepEqual(oldResult.failures, [])
  assert.ok(oldResult.notes.some((n) => n.includes('webview 段')))
  // 空闲记录的 WebView 预算 55 MiB（macOS）。本机门禁失败；--ci 下默认只告警，--webview-gate hard 才失败
  // （CI 的两个会话后窗口都传 hard）
  assert.deepEqual(
    evaluate(sampleIdle({ rss_webview: 52 }), sampleBaseline({ idle: null })).failures,
    [],
  )
  const fatWv = sampleIdle({ rss_webview: 58 })
  assert.ok(
    evaluate(fatWv, sampleBaseline({ idle: null })).failures.some((f) => f.includes('55 MiB')),
    '本机门禁：WebView 超 55 MiB 应失败',
  )
  const fatCi = evaluate(fatWv, sampleBaseline({ idle: null }), { ci: true })
  assert.deepEqual(fatCi.failures, [], 'CI 不带 --webview-gate hard 时只告警')
  assert.ok(fatCi.warnings.some((w) => w.includes('55 MiB') && w.includes('只告警')))
  assert.ok(
    evaluate(fatWv, sampleBaseline({ idle: null }), {
      ci: true,
      webviewGate: 'hard',
    }).failures.some((f) => f.includes('55 MiB')),
    'CI + --webview-gate hard：超 55 MiB 应失败',
  )
  // --require-webview（--webview-gate hard 隐含）：测量无效直接失败，不再悄悄退化成告警
  for (const [name, idle] of [
    ['近似归属', { webview_attribution: 'responsible-approx' }],
    [
      'WebView 没有测到',
      { rss_webview: null, cpu_webview_avg: null, cpu_total_avg: null, webview_attribution: null },
    ],
    ['进程不足 3 个', { webview_processes: 2 }],
  ]) {
    const invalid = sampleIdle({ webview_processes: 3, ...idle })
    for (const opts of [{ requireWebview: true }, { webviewGate: 'hard' }]) {
      assert.ok(
        evaluate(invalid, null, { ci: true, ...opts }).failures.some((f) =>
          f.includes('WebView 测量无效'),
        ),
        `${name}：${Object.keys(opts)[0]} 应失败`,
      )
    }
    // 不带 --require-webview 时 CI 照旧不失败
    assert.deepEqual(evaluate(invalid, null, { ci: true }).failures, [], `${name}：默认 CI 不失败`)
  }
  assert.ok(
    evaluate({ ...sampleIdle({ webview_processes: 3 }), build: 'debug' }, null, {
      ci: true,
      requireWebview: true,
    }).failures.some((f) => f.includes('release')),
    'macOS 的 WebView 内存测量必须是 release',
  )
  assert.deepEqual(
    evaluate(sampleIdle({ webview_processes: 3 }), null, { ci: true, requireWebview: true })
      .failures,
    [],
    '有效的测量通过',
  )
  // --expect-state：exercise 真的唤出了面板、采样窗口的状态与预期一致
  const session = (over) => ({
    settle_s: 60,
    window_s: [60, 120],
    traces: 4,
    shown: 2,
    recycles_s: [44.6],
    unloads_s: [],
    state: 'recycled',
    panel: 'hidden',
    views: ['input', 'clipboard'],
    hides: { toggle: 2 },
    last_hide_s: -1.2,
    clipboard: { text: 21, link: 4, color: 3, code: 2, image: 3 },
    log_lines: 0,
    log: 'wisp.log',
    ...over,
  })
  const withSession = (over) => sampleIdle({ webview_processes: 3, session: session(over) })
  assert.deepEqual(validateDocument(withSession({})).errors, [], 'idle.session 符合 schema')
  const expectRecycled = { ci: true, requireWebview: true, expectState: 'recycled' }
  const okState = evaluate(withSession({}), null, expectRecycled)
  assert.deepEqual(okState.failures, [])
  assert.ok(okState.passes.some((p) => p.includes('回收后')))
  for (const [name, stateRec, key] of [
    ['没有会话信息', sampleIdle({ webview_processes: 3 }), '没有会话信息'],
    ['读不到日志', withSession({ shown: null, traces: null, state: 'unknown' }), '读不到应用日志'],
    ['exercise 没有唤出面板', withSession({ shown: 0, traces: 0 }), 'exercise 没有唤出面板'],
    [
      '回收被关掉 / 改了时长：窗口仍是回收前',
      withSession({ recycles_s: [], state: 'hot' }),
      'panel_recycle_after',
    ],
    ['窗口内发生了回收', withSession({ recycles_s: [90], state: 'mixed' }), 'panel_recycle_after'],
    // 失焦先隐藏、再次 --trigger 把面板重新打开，exercise 结束时面板还在屏幕上
    [
      '采样窗口里面板还在屏幕上',
      withSession({
        panel: 'visible',
        views: ['input', 'input', 'clipboard'],
        hides: { blur: 1, toggle: 1 },
      }),
      '面板还在屏幕上',
    ],
    [
      '没有 debug 日志、无法核对面板状态',
      withSession({ panel: 'unknown', views: null, hides: null, clipboard: null }),
      '没有 debug 级别的行',
    ],
    // 旧记录没有 panel 字段：同样无法核对
    [
      '旧记录没有 panel 字段',
      (() => {
        const old = withSession({})
        for (const k of ['panel', 'views', 'hides', 'last_hide_s', 'clipboard', 'log_lines'])
          delete old.idle.session[k]
        return old
      })(),
      '没有 debug 级别的行',
    ],
  ]) {
    assert.ok(
      evaluate(stateRec, null, expectRecycled).failures.some((f) => f.includes(key)),
      `--expect-state recycled：${name}应失败`,
    )
  }
  const hotOver = evaluate(
    sampleIdle({
      rss_webview: 62.5,
      webview_processes: 3,
      session: session({ window_s: [8, 38], recycles_s: [44.6], state: 'hot' }),
    }),
    null,
    { ci: true, requireWebview: true, expectState: 'hot' },
  )
  assert.deepEqual(hotOver.failures, [], '回收前窗口：55 MiB 不适用，防退化上限 100 MiB 以内通过')
  assert.ok(
    hotOver.passes.some(
      (p) => p.includes('62.5 MiB（回收前）') && p.includes('防退化上限 100 MiB'),
    ),
  )
  // 回收前按防退化上限硬判（CI 传 --webview-gate hard），回收后按 55 MiB 硬判
  const hotRec = (mib) =>
    sampleIdle({
      rss_webview: mib,
      webview_processes: 3,
      session: session({ window_s: [8, 38], recycles_s: [44.6], state: 'hot' }),
    })
  const ciHard = { ci: true, requireWebview: true, expectState: 'hot', webviewGate: 'hard' }
  assert.deepEqual(evaluate(hotRec(90.6), null, ciHard).failures, [])
  assert.ok(
    evaluate(hotRec(104), null, ciHard).failures.some(
      (f) => f.includes('104 MiB（回收前）') && f.includes('防退化上限 100 MiB'),
    ),
    '回收前超过防退化上限：hard 时失败',
  )
  assert.ok(
    evaluate(hotRec(104), null).failures.some((f) => f.includes('防退化上限')),
    '本机门禁同样失败',
  )
  // Apple 虚拟机（macos-15 runner）没有 GPU 进程——2 个进程算有效；第一次唤出的 ready 超时只告警；内存照常硬判
  const vmRec = (mib, over = {}) => ({
    ...hotRec(mib),
    machine: 'VirtualMac2,1',
    idle: { ...hotRec(mib).idle, webview_processes: 2 },
    ...over,
  })
  assert.deepEqual(evaluate(vmRec(63), null, ciHard).failures, [], '虚拟机：2 个进程的有效测量')
  assert.ok(
    evaluate({ ...vmRec(63), machine: 'Mac15,6' }, null, ciHard).failures.some((f) =>
      f.includes('WebView 测量无效'),
    ),
    '真机：2 个进程仍判无效',
  )
  assert.ok(
    evaluate(vmRec(104), null, ciHard).failures.some((f) => f.includes('防退化上限 100 MiB')),
    '虚拟机：回收前内存照常硬判',
  )
  const vmLatency = {
    source: 'log',
    metric: 'prepare_to_shown_ms',
    count: 2,
    p50: 300,
    p95: 458,
    max: 458,
    ready_timeouts: 1,
    first_show: { count: 1, ready_timeouts: 1, not_shown: 0, max: 458 },
  }
  const vmSlow = evaluate({ ...vmRec(63), latency: vmLatency }, null, ciHard)
  assert.deepEqual(vmSlow.failures, [], '虚拟机：第一次唤出的 ready 超时只告警')
  assert.ok(
    vmSlow.warnings.some((w) => w.includes('Apple 虚拟机') && w.includes('启动后第一次唤出')),
  )
  assert.ok(
    evaluate(
      {
        ...vmRec(63),
        machine: 'Mac15,6',
        idle: { ...vmRec(63).idle, webview_processes: 3 },
        latency: vmLatency,
      },
      null,
      ciHard,
    ).failures.some((f) => f.includes('启动后第一次唤出')),
    '真机：第一次唤出的 ready 超时照常失败',
  )
  // Hyper-V 虚拟机（GitHub 的 Windows / Linux runner，machine = "Virtual Machine"）的唤出延迟同样只告警——panel-recycle 的
  // 「用过之后」一段（例如剪贴板视图 1 次 ready 超时）；内存与测量有效性照常判定
  const hyperV = (os, over = {}) => {
    const base = sampleIdle({
      memory_metric: 'rss',
      rss_main: 33.8,
      rss_webview: 60,
      webview_attribution: 'descendants',
      webview_processes: 7,
      wakeups_method: 'context-switches',
      launch_env: null,
      session: session({ window_s: [32, 154], recycles_s: [], state: 'hot' }),
    })
    return {
      ...base,
      os,
      machine: 'Virtual Machine',
      latency: {
        ...vmLatency,
        p50: 79,
        p95: 178,
        max: 178,
        ready_timeouts: 1,
        first_show: { count: 1, ready_timeouts: 0, not_shown: 0, max: 79 },
      },
      ...over,
    }
  }
  const d3Gate = {
    ci: true,
    requireWebview: true,
    expectState: 'hot',
    expectViews: ['input', 'clipboard'],
  }
  for (const os of ['windows', 'linux']) {
    assert.deepEqual(validateDocument(hyperV(os)).errors, [], `${os} 的 Hyper-V 样例符合 schema`)
    const midTimeout = evaluate(hyperV(os), null, d3Gate)
    assert.deepEqual(midTimeout.failures, [], `${os} Hyper-V：中途 1 次 ready 超时只告警`)
    assert.ok(
      midTimeout.warnings.some((w) => w.includes('ready 超时 1 次') && w.includes('Hyper-V')),
      `${os} Hyper-V：告警写明原因`,
    )
    const firstTimeout = evaluate(
      hyperV(os, {
        latency: {
          ...vmLatency,
          first_show: { count: 1, ready_timeouts: 1, not_shown: 0, max: 85 },
        },
      }),
      null,
      d3Gate,
    )
    assert.deepEqual(firstTimeout.failures, [], `${os} Hyper-V：第一次唤出的 ready 超时只告警`)
    assert.ok(
      evaluate(hyperV(os, { machine: 'XPS 13 9340' }), null, d3Gate).failures.some((f) =>
        f.includes('ready 超时 1 次，超过上限 0'),
      ),
      `${os} 真机：ready 超时照常失败`,
    )
    assert.ok(
      evaluate(hyperV(os, { os: 'macos' }), null, { ci: true }).failures.some((f) =>
        f.includes('ready 超时 1 次，超过上限 0'),
      ),
      'macOS 记录不按 Hyper-V 放宽（machine = "Virtual Machine" 也照常失败）',
    )
    const hyperVOverMain = hyperV(os)
    hyperVOverMain.idle = { ...hyperVOverMain.idle, rss_main: 64 }
    assert.ok(
      evaluate(hyperVOverMain, null, { ...d3Gate, ci: false }).failures.some((f) =>
        f.includes('空闲主进程 RSS 64 MiB'),
      ),
      `${os} Hyper-V：内存预算照常判定`,
    )
    assert.ok(
      evaluate(
        hyperV(os, { idle: { ...hyperV(os).idle, webview_processes: 1 } }),
        null,
        d3Gate,
      ).failures.some((f) => f.includes('WebView 测量无效')),
      `${os} Hyper-V：测量有效性照常判定`,
    )
  }
  const recycledOver = evaluate(
    sampleIdle({
      rss_webview: 57,
      webview_processes: 3,
      session: session({ window_s: [63, 125], recycles_s: [44.6], state: 'recycled' }),
    }),
    null,
    { ci: true, requireWebview: true, expectState: 'recycled', webviewGate: 'hard' },
  )
  assert.ok(
    recycledOver.failures.some((f) => f.includes('57 MiB（回收后）') && f.includes('预算 55 MiB')),
  )
  // 主进程内存门禁——启动环境（launch_env）与会话后窗口的主进程参考值（gate.session_rss_main）
  const memBase = (over = {}) =>
    sampleBaseline({
      os_version: '15.7.9',
      idle: { ...sampleBaseline().idle, rss_main: 12.7 },
      gate: { session_rss_main: 13.3 },
      ...over,
    })
  const memRec = (idleOver = {}, over = {}) => ({
    ...withSession({}),
    os_version: '15.7.9',
    ...over,
    idle: { ...withSession({}).idle, rss_main: 13.3, ...idleOver },
  })
  const ciPost = { ci: true, requireWebview: true, expectState: 'recycled', webviewGate: 'hard' }
  assert.deepEqual(validateDocument(memRec()).errors, [], 'idle.launch_env 符合 schema')
  assert.deepEqual(validateDocument(memBase()).errors, [], 'gate.session_rss_main 符合 schema')
  const memOk = evaluate(memRec(), memBase(), ciPost)
  assert.deepEqual(memOk.failures, [], 'runner 实测（13.3 MiB、launch_env=true）通过')
  assert.ok(memOk.passes.some((p) => p.includes('会话后主进程 RSS（回收后） 13.3')))
  assert.ok(memOk.passes.some((p) => p.includes('启动环境生效')))
  // 会话后窗口不与基线的空闲段（从未显示）比较 WebView / CPU：WebView 50 MiB、CPU 0.08 % 对基线 36 MiB、0.03 % 不算退化
  const notCompared = evaluate(
    memRec({ rss_webview: 50, cpu_avg: 0.08, cpu_total_avg: 0.09 }),
    memBase(),
    ciPost,
  )
  assert.deepEqual(notCompared.failures, [])
  assert.ok(![...notCompared.warnings].some((w) => w.includes('退化')))
  assert.ok(notCompared.notes.some((n) => n.includes('不与基线的空闲段')))
  // 丢了 MallocSpaceEfficient：日志 launch_env=false 直接失败；主进程回到约 21 MiB 同样失败（CI 硬门禁）
  assert.ok(
    evaluate(memRec({ launch_env: false }), memBase(), ciPost).failures.some((f) =>
      f.includes('MallocSpaceEfficient'),
    ),
    'launch_env=false 应失败',
  )
  assert.ok(
    evaluate(memRec({ rss_main: 21.1 }), memBase(), ciPost).failures.some((f) =>
      f.includes('会话后主进程 RSS（回收后） 退化'),
    ),
    '会话后主进程 21 MiB 应失败',
  )
  // 门限是参考值 +10 % 且至少 +0.5 MiB：13.3 → 14.63
  assert.deepEqual(evaluate(memRec({ rss_main: 14.6 }), memBase(), ciPost).failures, [])
  assert.ok(evaluate(memRec({ rss_main: 14.7 }), memBase(), ciPost).failures.length > 0)
  // 读不到 launch_env：CI 的会话后窗口（--require-webview）失败，手工测量只告警；从终端启动（近似归属）不核对
  const { launch_env: _dropped, ...noEnvIdle } = memRec().idle
  const noEnv = { ...memRec(), idle: noEnvIdle }
  assert.ok(evaluate(noEnv, memBase(), ciPost).failures.some((f) => f.includes('没有 launch_env')))
  const noEnvManual = evaluate(noEnv, memBase())
  assert.ok(!noEnvManual.failures.some((f) => f.includes('launch_env')))
  assert.ok(noEnvManual.warnings.some((w) => w.includes('没有 launch_env')))
  const terminal = evaluate(
    { ...noEnv, idle: { ...noEnvIdle, webview_attribution: 'responsible-approx' } },
    memBase(),
  )
  assert.ok(![...terminal.failures, ...terminal.warnings].some((m) => m.includes('launch_env')))
  // 基线没有会话后的参考值：告警（主进程只剩绝对预算）；跨系统大版本：只告警
  assert.ok(
    evaluate(memRec(), memBase({ gate: {} }), ciPost).warnings.some((w) =>
      w.includes('gate.session_rss_main'),
    ),
  )
  const memCross = evaluate(memRec({ rss_main: 21.1 }), memBase({ os_version: '26.4' }), ciPost)
  assert.ok(!memCross.failures.some((f) => f.includes('会话后主进程')), '跨系统大版本不硬判')
  assert.ok(memCross.warnings.some((w) => w.includes('跨系统大版本')))
  // 从未显示的空闲记录：主进程与基线空闲段的比较在 CI 上同样硬判（经 LaunchServices 启动的 release）
  const neverBase = sampleBaseline({
    os_version: '26.4',
    idle: { ...sampleBaseline().idle, rss_main: 13.2 },
  })
  const never = (mib) =>
    sampleBaseline({ os_version: '26.4', idle: { ...sampleBaseline().idle, rss_main: mib } })
  assert.deepEqual(evaluate(never(14.4), neverBase, { ci: true }).failures, [])
  assert.ok(
    evaluate(never(21.4), neverBase, { ci: true }).failures.some((f) =>
      f.includes('空闲主进程 RSS 退化'),
    ),
    '从未显示 21 MiB 对基线 13.2 MiB：CI 上同样失败',
  )
  // 基线的空闲段本身是会话后窗口（runner 基线取 ② 回收后，带 idle.session）——从未显示的记录只比较主进程
  const postBase = memBase({
    idle: {
      ...sampleBaseline().idle,
      rss_main: 12.7,
      rss_webview: 40.4,
      webview_processes: 2,
      cpu_avg: 0.017,
      cpu_webview_avg: 0,
      cpu_total_avg: 0.017,
      wakeups: 0.02,
      wakeups_webview: 0,
      session: session({}),
    },
  })
  assert.deepEqual(validateDocument(postBase).errors, [], '基线空闲段带 idle.session 符合 schema')
  const neverRec = (idleOver = {}) => ({
    ...sampleBaseline({ os_version: '15.7.9' }),
    idle: {
      ...sampleBaseline().idle,
      rss_main: 12.5,
      rss_webview: 49.2,
      webview_processes: 3,
      ...idleOver,
    },
  })
  // 本机口径（不带 --ci，全部硬判）：WebView 49.2 对 40.4 MiB（+22 %）、CPU 0.04 对 0.017 %、唤醒 4.2 对 0.02 次/s 都不比较
  const neverVsPost = evaluate(neverRec(), postBase)
  assert.deepEqual(neverVsPost.failures, [], '从未显示对会话后基线：WebView / CPU / 唤醒不比较')
  assert.ok(![...neverVsPost.warnings].some((w) => w.includes('退化')))
  assert.ok(neverVsPost.notes.some((n) => n.includes('不是「从未显示」')))
  assert.ok(neverVsPost.passes.some((p) => p.includes('空闲主进程 RSS 12.5')))
  // 主进程照常比较（门限 12.7 → 13.97）：CI 上同样硬判
  assert.deepEqual(evaluate(neverRec({ rss_main: 13.9 }), postBase, { ci: true }).failures, [])
  assert.ok(
    evaluate(neverRec({ rss_main: 21.1 }), postBase, { ci: true }).failures.some((f) =>
      f.includes('空闲主进程 RSS 退化'),
    ),
    '从未显示 21 MiB 对会话后基线：主进程照常失败',
  )
  // 对照：同一记录对从未显示的基线（没有 idle.session）照常比较 WebView，本机口径下失败
  const { session: _s, ...neverIdle } = postBase.idle
  assert.ok(
    evaluate(neverRec(), memBase({ idle: neverIdle })).failures.some((f) =>
      f.includes('空闲 WebView RSS 退化'),
    ),
    '从未显示的基线：WebView 照常比较',
  )
  // 仓库里的基线（scripts/perf/baselines/）：符合 schema；runner 基线的空闲段标着会话后的状态，产生它的两份会话后记录
  // （examples/perf-post-*.macos-15.json；录制时 record.mjs 还不写 launch_env，同一次运行的应用日志里是 launch_env=true，
  // 这里补上）按 CI 的参数对它通过
  const baselineJson = (name) =>
    JSON.parse(readFileSync(new URL(`./baselines/${name}`, import.meta.url), 'utf8'))
  const runnerBase = baselineJson('macos-15.json')
  for (const name of ['macos.json', 'macos-15.json']) {
    assert.deepEqual(validateDocument(baselineJson(name)).errors, [], name)
  }
  assert.equal(runnerBase.idle.session?.state, 'recycled', 'runner 基线的空闲段是回收后窗口')
  assert.equal(baselineJson('macos.json').idle.session ?? null, null, '本机基线是从未显示')
  for (const [file, state] of [
    ['perf-post-hot.macos-15.json', 'hot'],
    ['perf-post-recycled.macos-15.json', 'recycled'],
  ]) {
    const ciRec = example(file)
    ciRec.idle.launch_env = true
    const res = evaluate(ciRec, runnerBase, {
      ci: true,
      requireWebview: true,
      expectState: state,
      expectViews: ['input', 'clipboard'],
      webviewGate: 'hard',
    })
    assert.deepEqual(res.failures, [], `${file} 对 runner 基线`)
    assert.ok(res.passes.some((p) => p.includes('会话后主进程 RSS')))
  }
  // --expect-views：每个视图至少上屏一次；次数不限（被重新打开时多关一次）
  const views = (record, list) =>
    evaluate(record, null, { ci: true, requireWebview: true, expectViews: list })
  assert.deepEqual(
    views(withSession({ views: ['input', 'input', 'clipboard'] }), ['input', 'clipboard']).failures,
    [],
  )
  assert.ok(
    views(withSession({ views: ['input'] }), ['input', 'clipboard']).failures.some((f) =>
      f.includes('没有用到 clipboard'),
    ),
  )
  assert.ok(
    views(withSession({ views: null }), ['input']).failures.some((f) =>
      f.includes('没有上屏视图的信息'),
    ),
  )
  assert.ok(
    views(sampleIdle({ webview_processes: 3 }), ['input']).failures.some((f) =>
      f.includes('没有上屏视图的信息'),
    ),
  )

  // runner 基线放宽 ready 超时上限时，撞上超时、没等兜底上屏的那一次不再算「连续触发不完整」
  {
    const midTimeout = sampleSmoke({ 'triggers.shown': 99, 'triggers.ready_timeouts': 1 })
    const relaxed = evaluate(midTimeout, sampleBaseline({ gate: { max_ready_timeouts: 2 } }), {
      ci: true,
    })
    assert.deepEqual(relaxed.failures, [], 'runner 基线放宽到 2：中途 1 次 ready 超时通过')
    assert.ok(
      evaluate(midTimeout, sampleBaseline(), { ci: true }).failures.some((f) =>
        f.includes('ready 超时 1 次'),
      ),
      '默认上限 0：照常失败',
    )
    assert.ok(
      evaluate(sampleSmoke({ 'triggers.shown': 99 }), sampleBaseline(), { ci: true }).failures.some(
        (f) => f.includes('连续触发不完整'),
      ),
      '既没有显示也没有超时：不完整',
    )
  }
  // 第一次唤出的 ready 超时不受 gate.max_ready_timeouts 放宽
  const lenient = sampleBaseline({ gate: { max_ready_timeouts: 2 } })
  assert.deepEqual(
    evaluate(
      sampleSmoke({ 'triggers.ready_timeouts': 1, 'triggers.last.ready_timeout': true }),
      lenient,
      { ci: true },
    ).failures,
    [],
    '放宽之后中途的 ready 超时可以通过',
  )
  for (const patch of [
    { 'first_translate.ready_timeout': true },
    { 'triggers.ready_timeouts': 1, 'triggers.first.ready_timeout': true },
  ]) {
    assert.ok(
      evaluate(sampleSmoke(patch), lenient, { ci: true }).failures.some((f) =>
        f.includes('启动后第一次唤出撞上 ready 超时'),
      ),
      `第一次唤出 ready 超时应失败：${Object.keys(patch).join(',')}`,
    )
  }

  // 性能记录的 latency 段：冷唤出、拆分、启动后第一次唤出、Overlay；CI 只告警（第一次唤出的超时除外）
  const recLat = (latency) => {
    const r = example('perf-record.example.json')
    return { ...r, latency: { ...r.latency, ...latency } }
  }
  const recBase = { ...example('perf-record.example.json'), latency: null }
  assert.deepEqual(evaluate(recLat({}), recBase).failures, [], '样例记录应通过')
  const latCases = [
    [
      '冷唤出超 150 ms',
      { pages: { cold: { count: 3, p50: 120, p95: 190, max: 190, ready_timeouts: 0 } } },
      '冷唤出',
    ],
    [
      'ready→成为 key 超 100 ms',
      {
        split: {
          ...recLat({}).latency.split,
          ready_to_key: { count: 9, p50: 40, p95: 130, max: 140 },
        },
      },
      'ready→成为 key',
    ],
    [
      'ready→窗口上屏超 100 ms',
      {
        split: {
          ...recLat({}).latency.split,
          ready_to_window: { count: 9, p50: 40, p95: 101, max: 120 },
        },
      },
      'ready→原生窗口上屏',
    ],
    [
      '第一次唤出太慢',
      { first_show: { count: 2, ready_timeouts: 0, not_shown: 0, max: 160 } },
      '启动后第一次唤出 prepare→shown',
    ],
    ['Overlay 上屏超 150 ms', { overlay_count: 5, overlay_p95: 180 }, 'Overlay'],
  ]
  // 冷唤出有卡片上屏分项：卡片上屏 ≤ 150 ms 时内容首帧超 150 只告警；卡片上屏超了才失败
  const coldCard = (cardP95, contentP95) =>
    recLat({
      pages: {
        cold: {
          count: 3,
          p50: 120,
          p95: contentP95,
          max: contentP95,
          ready_timeouts: 0,
          window: { count: 3, p50: 40, p95: cardP95, max: cardP95 },
        },
      },
    })
  const cardOk = evaluate(coldCard(58, 230), recBase)
  assert.deepEqual(cardOk.failures, [], '卡片上屏达标时内容首帧超 150 ms 不失败')
  assert.ok(
    cardOk.warnings.some((w) => w.includes('内容首帧')),
    '内容首帧超 150 ms 应告警',
  )
  assert.ok(
    evaluate(coldCard(160, 230), recBase).failures.some((f) => f.includes('卡片上屏')),
    '卡片上屏超 150 ms 应失败',
  )
  for (const [name, latency, key] of latCases) {
    const latRec = recLat(latency)
    assert.ok(
      evaluate(latRec, recBase).failures.some((f) => f.includes(key)),
      `${name}：本机门禁应失败`,
    )
    const ciResult = evaluate(latRec, recBase, { ci: true })
    assert.deepEqual(ciResult.failures, [], `${name}：CI 不应失败`)
    assert.ok(
      ciResult.warnings.some((w) => w.includes(key)),
      `${name}：CI 应告警`,
    )
  }
  const firstTimeout = recLat({
    ready_timeouts: 1,
    first_show: { count: 2, ready_timeouts: 1, not_shown: 0, max: 60 },
  })
  assert.ok(
    evaluate(
      firstTimeout,
      { ...recBase, gate: { max_ready_timeouts: 3 } },
      { ci: true },
    ).failures.some((f) => f.includes('次 ready 超时（不受 gate.max_ready_timeouts 放宽）')),
    '启动后第一次唤出的 ready 超时在 CI 上同样失败',
  )
  // 第一次唤出没有显示（真实日志里可能是 NotReady）：只告警，本机门禁也不失败
  const firstHidden = evaluate(
    recLat({ first_show: { count: 2, ready_timeouts: 0, not_shown: 1, max: 40 } }),
    recBase,
  )
  assert.deepEqual(firstHidden.failures, [])
  assert.ok(firstHidden.warnings.some((w) => w.includes('没有显示面板')))
  // 只有截图追踪（没有面板显示）的日志：Overlay 照样判定，不报「没有任何测量数据」
  const overlayOnly = {
    ...recBase,
    idle: null,
    smoke: null,
    latency: { source: 'log', count: 0, p50: null, p95: null, overlay_count: 3, overlay_p95: 90 },
  }
  const overlayOnlyResult = evaluate(overlayOnly, null)
  assert.deepEqual(overlayOnlyResult.failures, [])
  assert.ok(overlayOnlyResult.passes.some((p) => p.includes('Overlay 上屏')))

  // 构建类型分开比较：release 报告 + 只有 debug 冒烟段的基线 → CI 的 macOS 失败（不再静默跳过）
  const onlyDebug = sampleBaseline({ smoke: { debug: sampleBaseline().smoke.debug } })
  const mixed = evaluate(sampleSmoke({ 'rss_kb.end': 80 * 1024 }), onlyDebug, { ci: true })
  assert.ok(
    mixed.failures.some((f) => f.includes('smoke.release')),
    '缺少同构建类型的基线应失败',
  )
  // 基线冒烟段与本次的系统大版本不同（本机 26.x 录的基线、runner 15.x）：RSS 退化只告警
  const crossBase = sampleBaseline({
    smoke: {
      ...sampleBaseline().smoke,
      release: { ...sampleBaseline().smoke.release, os_version: '26.4.0' },
    },
  })
  const fatRelease = { 'rss_kb.end': 60 * 1024 }
  assert.ok(
    evaluate(sampleSmoke({ ...fatRelease, os_version: '26.5.1' }), crossBase, {
      ci: true,
    }).failures.some((f) => f.includes('冒烟主进程 RSS')),
    '同一系统大版本：RSS 退化照常失败',
  )
  const crossOs = evaluate(sampleSmoke({ ...fatRelease, os_version: '15.7' }), crossBase, {
    ci: true,
  })
  assert.deepEqual(crossOs.failures, [], '跨系统大版本：RSS 退化只告警')
  assert.ok(crossOs.warnings.some((w) => w.includes('跨系统大版本')))
  // debug 报告与 debug 基线比较（128 MiB 对 125 MiB 在 10% 以内），且只告警 debug 的宽松上限
  const debugRep = sampleSmoke({ debug_build: true, 'rss_kb.end': 128 * 1024 })
  const debugResult = evaluate(debugRep, sampleBaseline())
  assert.deepEqual(debugResult.failures, [], 'debug 报告与 debug 基线比较应通过')
  assert.ok(debugResult.passes.some((p) => p.includes('（debug）')))
  // 基线文件缺失：CI 的 macOS 告警门禁未生效；Linux 只记一条说明
  const noBase = evaluate(sampleSmoke(), null, { ci: true, baselineExpected: true })
  assert.deepEqual(noBase.failures, [])
  assert.ok(noBase.warnings.some((w) => w.includes('RSS 退化门禁没有生效')))
  const linuxNoBase = evaluate(sampleSmoke({ os: 'linux' }), null, {
    ci: true,
    baselineExpected: true,
  })
  assert.deepEqual(linuxNoBase.warnings, [])

  // Linux 的 RSS 退化在 CI 上只告警，在本机门禁里失败
  const linuxRep = sampleSmoke({
    os: 'linux',
    debug_build: true,
    memory_metric: 'rss',
    'rss_kb.end': 200 * 1024,
  })
  const linuxBase = sampleBaseline({
    os: 'linux',
    smoke: { debug: { cold_start_ms: 300, rss_main: 150, prepare_to_shown_p95: 40 } },
  })
  const ciLinux = evaluate(linuxRep, linuxBase, { ci: true })
  assert.deepEqual(ciLinux.failures, [])
  assert.ok(ciLinux.warnings.some((w) => w.includes('RSS')))
  assert.ok(evaluate(linuxRep, linuxBase).failures.some((f) => f.includes('RSS')))

  // 空闲记录：噪声下限以内不算退化（0.03% → 0.038%：+27%，但只多 0.008 个百分点）；超过下限的小值退化要拦下
  // （0.03% → 0.049%）；基线的 gate.floor 可以放宽下限
  assert.deepEqual(
    evaluate(sampleIdle({ cpu_avg: 0.038, cpu_total_avg: 0.048 }), sampleBaseline()).failures,
    [],
  )
  const smallCpu = sampleIdle({ cpu_avg: 0.049, cpu_total_avg: 0.059 })
  assert.ok(
    evaluate(smallCpu, sampleBaseline()).failures.some((f) => f.includes('空闲主进程 CPU 退化')),
  )
  assert.deepEqual(
    evaluate(smallCpu, sampleBaseline({ gate: { floor: { cpu: 0.03 } } })).failures,
    [],
  )
  // 有人在用（idle.activity）：开发机上 4 Hz 活跃档 + 复制把空闲 CPU 抬到 0.108%、主进程 +1037%——
  // CPU / 唤醒的预算与退化比较降为告警并说明原因；RSS 照常判定；没人在用（慢档、没有复制）时照常失败
  const busyRec = sampleIdle({
    cpu_avg: 0.091,
    cpu_webview_avg: 0.017,
    cpu_total_avg: 0.108,
    wakeups: 4.2,
    activity: {
      tiers_s: { active: 120, quiet: 0, away: 0, screens_idle: 0 },
      copies: 3,
      user_active_s: 120,
    },
  })
  assert.deepEqual(validateDocument(busyRec).errors, [], 'idle.activity 符合 schema')
  const busyRes = evaluate(busyRec, sampleBaseline())
  assert.deepEqual(busyRes.failures, [], '有人在用：CPU 只告警')
  assert.ok(busyRes.warnings.some((w) => w.includes('有人在用') && w.includes('复制 3 次')))
  assert.ok(busyRes.warnings.some((w) => w.includes('空闲主进程 CPU 退化')))
  const quietRec = sampleIdle({
    ...busyRec.idle,
    activity: {
      tiers_s: { active: 0, quiet: 0, away: 120, screens_idle: 0 },
      copies: 0,
      user_active_s: 0,
    },
  })
  assert.ok(
    evaluate(quietRec, sampleBaseline()).failures.some((f) => f.includes('空闲主进程 CPU 退化')),
  )
  assert.ok(
    evaluate(sampleIdle({ ...busyRec.idle, rss_main: 60 }), sampleBaseline()).failures.some((f) =>
      f.includes('空闲主进程 RSS'),
    ),
    '有人在用不放宽内存',
  )
  // 主进程唤醒预算：Windows / Linux 基线为 0 时新增 0.4 Hz 的定时器要拦下（预算与噪声下限两条都会命中）；
  // macOS 超过 5 次/s 失败；10% 以内的波动通过；CI 只告警
  const linuxIdle = (wakeups) =>
    sampleBaseline({ os: 'linux', idle: { ...sampleBaseline().idle, wakeups, wakeups_webview: 0 } })
  const linuxTimer = evaluate(linuxIdle(0.4), linuxIdle(0))
  assert.ok(linuxTimer.failures.some((f) => f.includes('找出新增的定时器')))
  assert.ok(linuxTimer.failures.some((f) => f.includes('空闲主进程唤醒 退化')))
  assert.deepEqual(evaluate(linuxIdle(0.1), linuxIdle(0)).failures, [])
  assert.ok(
    evaluate(sampleIdle({ wakeups: 6 }), sampleBaseline()).failures.some((f) =>
      f.includes('找出新增的定时器'),
    ),
  )
  assert.deepEqual(evaluate(sampleIdle({ wakeups: 4.6 }), sampleBaseline()).failures, [])
  const ciTimer = evaluate(linuxIdle(0.4), linuxIdle(0), { ci: true })
  assert.deepEqual(ciTimer.failures, [])
  assert.ok(ciTimer.warnings.some((w) => w.includes('找出新增的定时器')))
  // 主进程空闲，但 WebView 里泄漏了 rAF / 动画：合计超预算、WebView CPU 退化
  const wvLeak = sampleIdle({ cpu_avg: 0.02, cpu_webview_avg: 0.4, cpu_total_avg: 0.42 })
  const wvLeakResult = evaluate(wvLeak, sampleBaseline())
  assert.ok(wvLeakResult.failures.some((f) => f.includes('空闲 CPU 合计')))
  assert.ok(wvLeakResult.failures.some((f) => f.includes('空闲 WebView CPU 退化')))
  assert.deepEqual(evaluate(wvLeak, sampleBaseline(), { ci: true }).failures, [])
  // WebView 没有测到：本机门禁失败（不能当成 0 MiB 放行），CI 告警
  const noWv = sampleIdle({
    rss_webview: null,
    cpu_webview_avg: null,
    cpu_total_avg: null,
    webview_attribution: null,
    notes: ['采样期间没有找到属于 pid 1 的 WebKit 进程'],
  })
  assert.ok(
    evaluate(noWv, sampleBaseline()).failures.some((f) => f.includes('WebView 进程没有测到')),
  )
  assert.ok(evaluate(noWv, sampleBaseline(), { ci: true }).warnings.length > 0)
  // 近似归属只告警
  const approx = evaluate(
    sampleIdle({ webview_attribution: 'responsible-approx' }),
    sampleBaseline(),
  )
  assert.deepEqual(approx.failures, [])
  assert.ok(approx.warnings.some((w) => w.includes('近似')))
  // 基线与报告的平台不一致
  assert.ok(evaluate(sampleSmoke(), sampleBaseline({ os: 'linux' })).failures.length > 0)
  ok(`smoke-gate 自检通过（${cases.length + latCases.length + 98} 组）`)
}

function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      ci: { type: 'boolean', default: false },
      'expect-triggers': { type: 'string', default: '100' },
      'require-webview': { type: 'boolean', default: false },
      'expect-state': { type: 'string' },
      'expect-views': { type: 'string' },
      'webview-gate': { type: 'string', default: 'warn' },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log(
      '用法：node scripts/perf/smoke-gate.mjs <report.json> [baseline.json] [--ci] [--expect-triggers N]\n' +
        '        [--require-webview] [--expect-state hot|recycled|unloaded] [--expect-views v1,v2] [--webview-gate warn|hard]\n' +
        '        | --self-test',
    )
    return
  }
  if (values['self-test']) return selfTest()
  const [reportPath, baselinePath] = positionals
  if (!reportPath)
    throw new Error('缺少报告路径（target/smoke-report.json 或 measure-*.sh 写出的性能记录）')
  if (!existsSync(reportPath)) {
    throw new Error(`报告不存在：${reportPath}（WISP_SMOKE 没有写出报告——检查 smoke-run.log）`)
  }
  const rep = readJsonFile(reportPath, '报告')
  let baseline = null
  if (baselinePath) {
    if (existsSync(baselinePath)) baseline = readJsonFile(baselinePath, '基线')
    else info(`基线 ${baselinePath} 不存在：只检查不依赖基线的项`)
  }
  const expectTriggers = Number(values['expect-triggers'])
  if (!Number.isInteger(expectTriggers) || expectTriggers < 1) {
    throw new Error('--expect-triggers 必须是正整数')
  }
  const expectState = values['expect-state']
  if (expectState !== undefined && !['hot', 'recycled', 'unloaded'].includes(expectState)) {
    throw new Error('--expect-state 只能是 hot / recycled / unloaded')
  }
  const webviewGate = values['webview-gate']
  if (!['warn', 'hard'].includes(webviewGate)) throw new Error('--webview-gate 只能是 warn / hard')
  const result = evaluate(rep, baseline, {
    ci: values.ci,
    expectTriggers,
    baselineExpected: Boolean(baselinePath) && !baseline,
    requireWebview: values['require-webview'],
    expectState,
    expectViews: values['expect-views']
      ?.split(',')
      .map((v) => v.trim())
      .filter(Boolean),
    webviewGate,
  })
  printResult(result, `性能门禁：${reportPath}${values.ci ? '（CI 模式）' : ''}`)
  if (result.failures.length) {
    throw new Error(
      `门禁未通过：${result.failures.length} 项失败、${result.warnings.length} 项告警`,
    )
  }
  ok(`门禁通过${result.warnings.length ? `（${result.warnings.length} 项告警）` : ''}`)
}

if (isMain(import.meta.url)) await run(main)
