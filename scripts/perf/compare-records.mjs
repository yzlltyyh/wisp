#!/usr/bin/env node
/**
 * scripts/perf/compare-records.mjs：把几份性能记录（wisp.perf/1）的空闲段并排列成一张表——会话后测量的
 * 「回收前 / 回收后」（macOS 冒烟作业）、面板回收对照的「从未显示 / 用过之后 / 用过并回收」（panel-recycle 作业）都用它出摘要。
 *
 * 用法：
 *   node scripts/perf/compare-records.mjs <标签>=<记录.json>… [--title <标题>]   打印表格并写进 Actions 摘要
 *   node scripts/perf/compare-records.mjs --self-test
 *
 * 只汇报、不判定（判定由 smoke-gate.mjs 完成）；记录不存在时那一行写「没有记录」（前面的测量失败时照样出表）。表后列出
 * 各记录的说明（idle.notes：exercise、更新检查是否落在窗口里、采样终点等）与 exercise 的使用画像（唤出的视图、剪贴板条目）。
 */
import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { parseArgs } from 'node:util'
import { appendSummary, info, isMain, ok, run } from '../lib/common.mjs'

const STATE = {
  hot: '回收前',
  recycled: '回收后',
  unloaded: '卸载后',
  mixed: '窗口内回收 / 卸载',
  unknown: '未知',
}

const dash = (v, unit = '') => (v === null || v === undefined ? '—' : `${v}${unit}`)

/** 一份记录 → 表格的一行（列见 HEADER）；record 为 null 时整行是「没有记录」 */
export function row(label, record) {
  const i = record?.idle
  if (!i) return [label, '没有记录', '—', '—', '—', '—', '—', '—']
  const s = i.session
  const panel = { hidden: '；面板已隐藏', visible: '；**面板仍在屏幕上**', unknown: '' }[
    s?.panel ?? 'unknown'
  ]
  const shown = s?.views ? `${s.views.length} 次（${s.views.join('、')}）` : `${dash(s?.shown)} 次`
  const session = s
    ? `${STATE[s.state] ?? s.state}；exercise 后 ${s.window_s.join('–')} s；唤出 ${shown}${panel}`
    : '没有 exercise'
  return [
    label,
    session,
    dash(i.rss_main, ' MiB'),
    `${dash(i.rss_webview, ' MiB')}（${dash(i.webview_processes)} 个）`,
    dash(i.cpu_avg, '%'),
    dash(i.cpu_webview_avg, '%'),
    dash(i.wakeups, ' 次/s'),
    dash(i.wakeups_webview, ' 次/s'),
  ]
}

export const HEADER = [
  '测量',
  '状态',
  '主进程内存',
  'WebView 合计',
  '主进程 CPU',
  'WebView CPU',
  '主进程唤醒',
  'WebView 唤醒',
]

/** 一份记录的说明：exercise 的使用画像 + idle.notes（没有时为 null） */
export function notesOf(label, record) {
  const i = record?.idle
  if (!i) return null
  const parts = []
  const s = i.session
  if (s?.clipboard) {
    const clips = Object.entries(s.clipboard).map(([k, n]) => `${k} ${n}`)
    const hides = Object.entries(s.hides ?? {}).map(([k, n]) => `${k} ×${n}`)
    parts.push(
      `exercise：剪贴板记录 ${clips.join('、') || '0'}；隐藏 ${hides.join('、') || '—'}；采样窗口里日志 ${dash(s.log_lines)} 行`,
    )
  }
  parts.push(...(i.notes ?? []))
  return parts.length ? `- **${label}**：${parts.join('；')}` : null
}

export function markdown(title, rows, notes = []) {
  const lines = [
    `### ${title}`,
    '',
    `| ${HEADER.join(' | ')} |`,
    `| ${HEADER.map((_, k) => (k < 2 ? '---' : '---:')).join(' | ')} |`,
    ...rows.map((r) => `| ${r.join(' | ')} |`),
  ]
  const n = notes.filter(Boolean)
  if (n.length) lines.push('', ...n)
  return lines.join('\n')
}

function selfTest() {
  const rec = {
    idle: {
      rss_main: 22.7,
      rss_webview: 52.4,
      webview_processes: 3,
      cpu_avg: 0.02,
      cpu_webview_avg: 0,
      wakeups: 1.3,
      wakeups_webview: 0.1,
      session: { state: 'recycled', window_s: [65, 125], shown: 2 },
    },
  }
  assert.deepEqual(row('回收后', rec), [
    '回收后',
    '回收后；exercise 后 65–125 s；唤出 2 次',
    '22.7 MiB',
    '52.4 MiB（3 个）',
    '0.02%',
    '0%',
    '1.3 次/s',
    '0.1 次/s',
  ])
  assert.equal(row('从未显示', { idle: { ...rec.idle, session: undefined } })[1], '没有 exercise')
  const used = {
    idle: {
      ...rec.idle,
      notes: ['更新检查：本次运行没有发生'],
      session: {
        ...rec.idle.session,
        panel: 'hidden',
        views: ['input', 'clipboard'],
        hides: { toggle: 2 },
        clipboard: { text: 30, image: 3 },
        log_lines: 1,
      },
    },
  }
  assert.equal(
    row('回收后', used)[1],
    '回收后；exercise 后 65–125 s；唤出 2 次（input、clipboard）；面板已隐藏',
  )
  assert.equal(
    notesOf('回收后', used),
    '- **回收后**：exercise：剪贴板记录 text 30、image 3；隐藏 toggle ×2；采样窗口里日志 1 行；更新检查：本次运行没有发生',
  )
  assert.equal(notesOf('缺失', null), null)
  assert.ok(
    markdown('t', [row('a', used)], [notesOf('a', used), null]).endsWith('本次运行没有发生'),
  )
  assert.equal(row('缺失', null)[1], '没有记录')
  const md = markdown('t', [row('a', rec)])
  assert.ok(md.includes('| --- | --- | ---: |'))
  ok('compare-records 自检通过')
}

function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      title: { type: 'string', default: '空闲测量对照' },
      'self-test': { type: 'boolean', default: false },
    },
  })
  if (values['self-test']) return selfTest()
  if (positionals.length === 0)
    throw new Error('用法：compare-records.mjs <标签>=<记录.json>… [--title <标题>]')
  const entries = positionals.map((arg) => {
    const at = arg.indexOf('=')
    if (at <= 0) throw new Error(`参数应为 <标签>=<记录.json>：${arg}`)
    const path = arg.slice(at + 1)
    return [arg.slice(0, at), existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')) : null]
  })
  const md = markdown(
    values.title,
    entries.map(([label, record]) => row(label, record)),
    entries.map(([label, record]) => notesOf(label, record)),
  )
  info(md)
  appendSummary(md)
}

if (isMain(import.meta.url)) await run(main)
