/**
 * 状态行提示的优先级（notes.ts）：正文区在识别进行中与思考耗尽时留空，其余按「已截断 > 部分区域未能识别 > 已编辑 >
 * 来自缓存 > 参数降级」取一条；译文区按「思考耗尽 > 已截断 > 原文已修改 > 来自缓存 > 参数降级」。
 */
import { describe, expect, it } from 'vitest'
import { isExhausted, mainNote, NOTE_SPECS, translationNote } from '../notes'
import type { DoneEvent, RunState } from '../useRun'

const state = (patch: Partial<RunState> = {}): RunState => ({
  key: 1,
  phase: 'done',
  requestId: 'r-1',
  start: null,
  meta: null,
  done: null,
  error: null,
  notices: [],
  hasText: true,
  longText: null,
  stage: null,
  ...patch,
})
const done = (patch: Partial<DoneEvent> = {}): DoneEvent => ({
  type: 'done',
  elapsed_ms: 900,
  usage: null,
  truncated: null,
  cached: false,
  no_text: false,
  ...patch,
})

const everything = state({
  done: done({ truncated: 'max_tokens', cached: true }),
  notices: ['partial', 'param_degraded'],
})
const exhausted = state({ done: done({ truncated: 'max_tokens' }), hasText: false })

describe('思考耗尽', () => {
  it('结束时停在输出上限且没有文字才算', () => {
    expect(isExhausted(exhausted)).toBe(true)
    expect(isExhausted(state({ done: done({ truncated: 'max_tokens' }) }))).toBe(false)
    expect(isExhausted({ ...exhausted, phase: 'cancelled' })).toBe(false)
    expect(isExhausted(state({ done: done(), hasText: false }))).toBe(false)
  })
})

describe('正文区', () => {
  it('识别进行中、思考耗尽时不提示', () => {
    expect(mainNote({ ...everything, phase: 'waiting' }, true)).toBeNull()
    expect(mainNote({ ...everything, phase: 'streaming' }, true)).toBeNull()
    expect(mainNote(exhausted, true)).toBeNull()
  })

  it('已截断 > 部分区域未能识别 > 已编辑 > 来自缓存 > 参数降级', () => {
    expect(mainNote(everything, true)).toBe('truncated')
    const noTrunc = { ...everything, done: done({ cached: true }) }
    expect(mainNote(noTrunc, true)).toBe('partial')
    const noPartial = { ...noTrunc, notices: ['param_degraded' as const] }
    expect(mainNote(noPartial, true)).toBe('edited')
    expect(mainNote(noPartial, false)).toBe('cached')
    expect(mainNote({ ...noPartial, done: done() }, false)).toBe('param_degraded')
    expect(mainNote(state({ done: done() }), false)).toBeNull()
  })

  it('取消后照样提示（已编辑）', () => {
    expect(mainNote(state({ phase: 'cancelled' }), true)).toBe('edited')
  })
})

describe('译文区', () => {
  it('思考耗尽 > 已截断 > 原文已修改 > 来自缓存 > 参数降级', () => {
    expect(translationNote(exhausted, true)).toBe('exhausted')
    expect(translationNote(everything, true)).toBe('truncated')
    const noTrunc = { ...everything, done: done({ cached: true }) }
    expect(translationNote(noTrunc, true)).toBe('stale')
    expect(translationNote(noTrunc, false)).toBe('cached')
    expect(translationNote({ ...noTrunc, done: done() }, false)).toBe('param_degraded')
    expect(translationNote(state({ done: done() }), false)).toBeNull()
  })

  it('「部分区域未能识别」只属于正文区', () => {
    expect(translationNote(state({ done: done(), notices: ['partial'] }), false)).toBeNull()
  })
})

describe('展示', () => {
  it('只有思考耗尽与已截断用警示色', () => {
    const warning = Object.entries(NOTE_SPECS)
      .filter(([, spec]) => 'tone' in spec && spec.tone === 'warning')
      .map(([kind]) => kind)
    expect(warning.sort()).toEqual(['exhausted', 'truncated'])
  })
})
