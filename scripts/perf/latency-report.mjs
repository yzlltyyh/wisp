#!/usr/bin/env node
/**
 * scripts/perf/latency-report.mjs：从 perf 日志或冒烟报告计算延迟的 p50 / p95。
 *
 * 用法：
 *   node scripts/perf/latency-report.mjs <wisp.log | smoke-report.json>…   打印摘要
 *     --json        输出 perf 记录的 latency 段（JSON），可直接放进性能记录
 *     --self-test   用内置样例自检（CI 用）
 *
 * 日志格式见 README「延迟日志格式」：应用的 `PerfMarks::finish` 每完成一次追踪写一行
 * `perf trace=… prepare_ms=… shown_ms=… first_token_ms=… ready_timeout=…`（也兼容 `perf latency {LatencyMarks JSON}`）。
 * 主指标是 prepare→shown（`shown_ms - prepare_ms`），另报告触发→shown 与首 token；面板没有显示的追踪不计入分位数。
 * 冒烟报告只有 p50 / p95 / max 汇总，直接读取。分位数用最近秩法。
 *
 * 另外按 `page=` 分组（热页面 / 回收出来的新页面 / 空闲卸载后重建的冷唤出，冷唤出不计入主指标、单独对照 150 ms 的
 * 冷唤出预算）、ready 之后的拆分（`window_ms` / `key_ms`）、每次启动后的第一次唤出、Overlay 上屏。
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { parseArgs } from 'node:util'
import { isMain, ok, run, warn } from '../lib/common.mjs'
import { BUDGET } from './smoke-gate.mjs'
import {
  SMOKE_SCHEMA,
  latencyFromMarks,
  latencyFromSmoke,
  parseLatencyLog,
  summarize,
  timedOutShown,
  validateDocument,
} from './lib.mjs'

/** 读取一个或多个输入，返回 `{ latency, extra, skipped }`；extra 是只用于打印的附加统计 */
export function latencyFromFiles(files) {
  const marks = []
  let skipped = 0
  for (const file of files) {
    const text = readFileSync(file, 'utf8')
    if (text.trimStart().startsWith('{')) {
      const doc = JSON.parse(text)
      if (doc.schema === SMOKE_SCHEMA) {
        const { errors } = validateDocument(doc)
        if (errors.length) throw new Error(`${file} 不符合冒烟报告格式：\n  ${errors.join('\n  ')}`)
        if (files.length > 1) throw new Error('冒烟报告只能单独统计，不能与日志混合')
        const t = doc.triggers
        return {
          latency: latencyFromSmoke(doc),
          extra: { triggerToShown: t.trigger_to_shown_ms, traces: t.count },
          skipped: 0,
        }
      }
    }
    const r = parseLatencyLog(text)
    marks.push(...r.marks)
    skipped += r.skipped
  }
  return {
    latency: latencyFromMarks(marks),
    extra: {
      triggerToShown: summarize(marks.map((m) => m.shown_ms)),
      traces: marks.length,
      timedOut: timedOutShown(marks),
    },
    skipped,
  }
}

function print({ latency, extra, skipped }) {
  const fmt = (v) => (v === null ? '—' : `${v} ms`)
  const source = latency.source === 'smoke' ? '冒烟报告' : '日志'
  const traces = extra && extra.traces !== latency.count ? `，共 ${extra.traces} 次追踪` : ''
  console.log(`面板显示          ${latency.count} 次（来源：${source}${traces}）`)
  console.log(
    `prepare→shown     p50 ${fmt(latency.p50)}  p95 ${fmt(latency.p95)}  max ${fmt(latency.max)}`,
  )
  if (extra) {
    const t = extra.triggerToShown
    console.log(`触发→shown        p50 ${fmt(t.p50)}  p95 ${fmt(t.p95)}  max ${fmt(t.max)}`)
  }
  if (latency.first_token_p50 !== null) {
    console.log(
      `首 token          p50 ${fmt(latency.first_token_p50)}  p95 ${fmt(latency.first_token_p95)}`,
    )
  }
  const late = extra?.timedOut ?? []
  console.log(
    `ready 超时        ${latency.ready_timeouts} 次${late.length ? `（之后显示出来的 ${late.length} 次：触发→显示 ${late.join(' / ')} ms，已计入分位数）` : ''}`,
  )
  if (latency.overlay_count) {
    console.log(
      `Overlay 上屏      ${latency.overlay_count} 次  p50 ${fmt(latency.overlay_p50)}  p95 ${fmt(latency.overlay_p95)}  max ${fmt(latency.overlay_max)}（预算 ${BUDGET.overlay_p95} ms）`,
    )
  }
  const pageLabel = { warm: '热页面', recycled: '回收后新页面', cold: '冷唤出（卸载后重建）' }
  for (const [page, p] of Object.entries(latency.pages ?? {})) {
    const budget = page === 'cold' ? BUDGET.cold_show_p95 : BUDGET.prepare_to_shown_p95
    console.log(
      `  page=${page.padEnd(9)}${p.count} 次  p50 ${fmt(p.p50)}  p95 ${fmt(p.p95)}  max ${fmt(p.max)}  ready 超时 ${p.ready_timeouts} 次（${pageLabel[page]}，预算 ${budget} ms${page === 'cold' ? '，不计入主指标' : ''}）`,
    )
    if (page === 'cold' && p.window?.count) {
      console.log(
        `    卡片上屏（prepare→window）${p.window.count} 次  p50 ${fmt(p.window.p50)}  p95 ${fmt(p.window.p95)}  max ${fmt(p.window.max)}（原生外壳，冷唤出的硬门禁按它判；上一行是内容首帧）`,
      )
    }
  }
  const split = latency.split
  if (split) {
    for (const [key, label] of [
      ['ready_to_window', 'ready→窗口上屏'],
      ['ready_to_key', 'ready→成为 key'],
      ['ready_to_shown', 'ready→shown'],
    ]) {
      const sp = split[key]
      if (sp?.count)
        console.log(
          `${label.padEnd(16)}${sp.count} 次  p50 ${fmt(sp.p50)}  p95 ${fmt(sp.p95)}  max ${fmt(sp.max)}`,
        )
    }
  }
  const fs = latency.first_show
  if (fs?.count) {
    console.log(
      `启动后第一次唤出  ${fs.count} 次  最长 ${fmt(fs.max)}  ready 超时 ${fs.ready_timeouts} 次  没有显示 ${fs.not_shown} 次`,
    )
  }
  if (skipped) warn(`${skipped} 行 perf 日志无法解析，已跳过`)
}

function selfTest() {
  const lines = []
  for (let i = 1; i <= 100; i++) {
    const marks = {
      session_id: i,
      origin: 'hotkey',
      method: null,
      selection_ms: null,
      prepare_ms: 2,
      ready_ms: 2 + i / 2,
      shown_ms: 2 + i,
      first_token_ms: i <= 50 ? 200 + i : null,
      ready_timeout: i === 100,
    }
    lines.push(
      `[2026-10-10][08:00:00][wisp_desktop_lib::app::perf][INFO] perf latency ${JSON.stringify(marks)}`,
    )
  }
  // PerfMarks::finish 的实际格式：没有到达的阶段写 -
  lines.push(
    '2026-10-10T12:45:24.705Z INFO  wisp_desktop_lib::app::perf: perf trace=101 origin=hotkey session=9 method=ax selection_ms=4 prepare_ms=1 ready_ms=3 shown_ms=4 first_token_ms=- ready_timeout=false',
    '2026-10-10T12:45:25.000Z INFO  wisp_desktop_lib::app::perf: perf trace=102 origin=cli session=- method=- selection_ms=- prepare_ms=- ready_ms=- shown_ms=- first_token_ms=- ready_timeout=true',
    '2026-10-10T12:45:25.100Z INFO  wisp_desktop_lib::app::perf: perf paste method=cg_event select_to_post_ms=12',
    '2026-10-10T12:45:25.200Z INFO  wisp_desktop_lib::app::bootstrap: startup stage=tray_ready elapsed_ms=152',
    '[INFO] perf latency {"shown_ms": }',
    '[INFO] 与性能无关的一行',
  )
  // 截图流程带 overlay_ms；ready 超时之后又显示出来的会话计入分位数
  lines.push(
    '2026-10-10T12:45:26.000Z INFO  wisp_desktop_lib::app::perf: perf trace=103 origin=hotkey session=- method=- selection_ms=- prepare_ms=- ready_ms=- shown_ms=- first_token_ms=- ready_timeout=false overlay_ms=96 selection=-',
    '2026-10-10T12:45:27.000Z INFO  wisp_desktop_lib::app::perf: perf trace=104 origin=hotkey session=- method=- selection_ms=- prepare_ms=- ready_ms=- shown_ms=- first_token_ms=- ready_timeout=false overlay_ms=140 selection=-',
  )
  const r = parseLatencyLog(lines.join('\r\n'))
  assert.equal(r.marks.length, 104)
  assert.equal(r.skipped, 1)
  assert.equal(r.marks[101].shown_ms, null)
  const l = latencyFromMarks(r.marks)
  // prepare→shown 为 1..100 与 3（第 102 次没有显示，不计入）：最近秩法 p50 = 第 51 小 = 50，p95 = 第 96 小 = 95
  assert.equal(l.count, 101)
  assert.equal(l.p50, 50)
  assert.equal(l.p95, 95)
  assert.equal(l.max, 100)
  assert.equal(l.ready_timeouts, 2)
  assert.equal(l.first_token_p50, 225)
  assert.equal(l.overlay_count, 2)
  assert.equal(l.overlay_p95, 140)
  assert.deepEqual(timedOutShown(r.marks), [102])
  assert.equal(summarize([]).p95, null)
  // 旧日志没有 page= / window_ms：全部计入主指标，分组为空，拆分没有样本；整份日志算一次启动
  assert.deepEqual(l.pages, {})
  assert.equal(l.split.ready_to_window.count, 0)
  assert.equal(l.split.ready_to_shown.count, 101)
  assert.deepEqual(l.first_show, { count: 1, ready_timeouts: 0, not_shown: 0, max: 1 })

  // 两次启动（stage 行分隔；第二次启动的 trace id 从 1 重来），page / window_ms / key_ms
  const perfLine = (id, prepare, ready, shown, extra) =>
    `2026-10-12T00:00:00.000Z INFO  wisp_desktop_lib::app::perf: perf trace=${id} origin=hotkey session=${id} method=- selection_ms=- prepare_ms=${prepare} ready_ms=${ready} shown_ms=${shown} first_token_ms=- ready_timeout=${extra.timeout ?? false} overlay_ms=- selection=- window_ms=${extra.window ?? '-'} key_ms=${extra.key ?? '-'} page=${extra.page ?? '-'}`
  const two = parseLatencyLog(
    [
      '2026-10-12T00:00:00.000Z INFO  wisp_desktop_lib::app::bootstrap: startup stage=tray_ready elapsed_ms=150',
      perfLine(1, 2, 60, 70, { timeout: true, window: 64, page: 'warm' }),
      perfLine(2, 2, 9, 14, { window: 11, key: 13, page: 'warm' }),
      perfLine(3, 1, 120, 131, { window: 125, key: 129, page: 'cold' }),
      perfLine(4, 2, 12, 20, { window: 15, page: 'recycled' }),
      '2026-10-12T01:00:00.000Z INFO  wisp_desktop_lib::app::bootstrap: startup stage=tray_ready elapsed_ms=140',
      perfLine(1, 3, 10, 16, { window: 12, key: 15, page: 'warm' }),
    ].join('\n'),
  )
  assert.deepEqual(
    two.marks.map((m) => m.launch),
    [0, 0, 0, 0, 1],
  )
  const w = latencyFromMarks(two.marks)
  // 主指标不含冷唤出：68、12、18、13 → p95 = 68
  assert.equal(w.count, 4)
  assert.equal(w.p95, 68)
  assert.deepEqual(w.pages.cold, {
    count: 1,
    p50: 130,
    p95: 130,
    max: 130,
    ready_timeouts: 0,
    // 卡片上屏（prepare→window_ms，原生外壳）：125 − 1
    window: { count: 1, p50: 124, p95: 124, max: 124 },
  })
  assert.equal(w.pages.warm.ready_timeouts, 1)
  assert.equal(w.pages.recycled.count, 1)
  assert.deepEqual(w.split.ready_to_window, { count: 5, p50: 3, p95: 5, max: 5 })
  assert.deepEqual(w.split.ready_to_key, { count: 3, p50: 5, p95: 9, max: 9 })
  assert.deepEqual(w.first_show, { count: 2, ready_timeouts: 1, not_shown: 0, max: 68 })
  // 有 stage 行时只按 stage 行分界：stage、trace 2、晚写出的 trace 1（长截图）、trace 3 是同一次启动；第一次唤出取
  // trace id 最小的那条（trace 1），不是行序上的第一条（回归用例）
  const stage =
    '2026-10-12T00:00:00.000Z INFO  wisp_desktop_lib::app::bootstrap: startup stage=tray_ready elapsed_ms=150'
  const outOfOrder = parseLatencyLog(
    [
      stage,
      perfLine(2, 2, 9, 14, {}),
      perfLine(1, 3, 80, 90, { timeout: true }),
      perfLine(3, 2, 9, 14, {}),
    ].join('\n'),
  )
  assert.deepEqual(
    outOfOrder.marks.map((m) => m.launch),
    [0, 0, 0],
  )
  assert.deepEqual(latencyFromMarks(outOfOrder.marks).first_show, {
    count: 1,
    ready_timeouts: 1,
    not_shown: 0,
    max: 87,
  })
  // 截图追踪（没有 prepare）不算第一次面板唤出，即使它的 trace id 最小
  const shotFirst = parseLatencyLog(
    [
      stage,
      perfLine(2, 2, 9, 14, {}),
      '2026-10-12T00:00:01.000Z INFO  wisp_desktop_lib::app::perf: perf trace=1 origin=hotkey session=- method=- selection_ms=- prepare_ms=- ready_ms=- shown_ms=- first_token_ms=- ready_timeout=false overlay_ms=90 selection=-',
    ].join('\n'),
  )
  assert.equal(latencyFromMarks(shotFirst.marks).first_show.max, 12)
  // 整个文件没有 stage 行时按 trace id 兜底：同一个 id 再次出现才算换了一次启动，晚写出的 trace 1 不算
  const noStage = parseLatencyLog(
    [
      perfLine(2, 2, 9, 14, {}),
      perfLine(1, 2, 9, 14, {}),
      perfLine(3, 2, 9, 14, {}),
      perfLine(1, 2, 9, 14, {}),
    ].join('\n'),
  )
  assert.deepEqual(
    noStage.marks.map((m) => m.launch),
    [0, 0, 0, 1],
  )
  assert.equal(latencyFromMarks(noStage.marks).first_show.count, 2)
  const late = parseLatencyLog(
    [perfLine(1, 2, 9, 14, {}), perfLine(3, 2, 9, 14, {}), perfLine(2, 2, 9, 14, {})].join('\n'),
  )
  assert.deepEqual(
    late.marks.map((m) => m.launch),
    [0, 0, 0],
  )
  ok('latency-report 自检通过')
}

function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      json: { type: 'boolean', default: false },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log(
      '用法：node scripts/perf/latency-report.mjs <日志 | smoke-report.json>… [--json] | --self-test',
    )
    return
  }
  if (values['self-test']) return selfTest()
  if (positionals.length === 0) throw new Error('缺少输入文件（perf 日志或 smoke-report.json）')
  const result = latencyFromFiles(positionals)
  if (result.latency.count === 0)
    throw new Error(
      '输入中没有面板显示过的 perf 追踪（perf trace=… 行；需要开发者模式或 debug 构建）',
    )
  if (values.json) console.log(JSON.stringify(result.latency, null, 2))
  else print(result)
}

if (isMain(import.meta.url)) await run(main)
