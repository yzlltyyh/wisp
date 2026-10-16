/**
 * OCR 结果文字三层的纯函数（model.ts）：按「初始排版 + 一串动作 → 显示」的状态表核对显示的文字、是否已编辑、是否在等
 * 排版结果；迟到的结果按 key 丢弃；换了一次结果，编辑与缓存作废、会话排版保留。
 */
import { describe, expect, it } from 'vitest'
import type { OcrLayout } from '../../../../shared/ipc/types'
import {
  canApply,
  initialText,
  reduceText,
  viewOf,
  type TextAction,
  type TextInput,
  type TextView,
} from '../model'

const RAW = '第一段\n\n第二段'
const TIGHT = '第一段\n第二段'
const ONE = '第一段第二段'
const at = (key = 1, applies = true, raw = RAW): TextInput => ({ key, raw, applies })

type Row = [
  name: string,
  initial: OcrLayout,
  actions: TextAction[],
  input: TextInput,
  expected: Partial<TextView>,
]

const ROWS: Row[] = [
  [
    '原样：显示识别文本，不等结果',
    'keep',
    [],
    at(),
    { layout: 'keep', display: RAW, pending: false, edited: false },
  ],
  [
    '默认去掉空行、结果还没到：先显示识别文本（不出现处理中）',
    'remove_blank_lines',
    [],
    at(),
    { layout: 'remove_blank_lines', display: RAW, pending: true },
  ],
  [
    '结果到了：显示排版结果',
    'remove_blank_lines',
    [{ type: 'laid', key: 1, layout: 'remove_blank_lines', text: TIGHT }],
    at(),
    { display: TIGHT, base: TIGHT, pending: false },
  ],
  [
    '排版不作用（结构化 / 直接翻译 / 还在识别）：显示识别文本',
    'join_lines',
    [{ type: 'laid', key: 1, layout: 'join_lines', text: ONE }],
    at(1, false),
    { layout: 'keep', display: RAW, pending: false },
  ],
  [
    '切到还没求过的排版：等结果期间继续显示切换前的文字',
    'remove_blank_lines',
    [
      { type: 'laid', key: 1, layout: 'remove_blank_lines', text: TIGHT },
      { type: 'layout', layout: 'join_lines', input: at() },
    ],
    at(),
    { layout: 'join_lines', display: TIGHT, pending: true, turned: true },
  ],
  [
    '切回原样：立即显示识别文本',
    'remove_blank_lines',
    [
      { type: 'laid', key: 1, layout: 'remove_blank_lines', text: TIGHT },
      { type: 'layout', layout: 'keep', input: at() },
    ],
    at(),
    { layout: 'keep', display: RAW, pending: false },
  ],
  [
    '编辑：显示编辑后的文字，与排版结果不同即「已编辑」',
    'keep',
    [{ type: 'edit', key: 1, text: '改过' }],
    at(),
    { display: '改过', edited: true, edit: '改过' },
  ],
  [
    '编辑后又改回排版结果：不算已编辑',
    'keep',
    [{ type: 'edit', key: 1, text: RAW }],
    at(),
    { display: RAW, edited: false },
  ],
  [
    '放弃修改：回到排版结果',
    'remove_blank_lines',
    [
      { type: 'laid', key: 1, layout: 'remove_blank_lines', text: TIGHT },
      { type: 'edit', key: 1, text: '改过' },
      { type: 'discard', key: 1 },
    ],
    at(),
    { display: TIGHT, edited: false, edit: null, turned: true },
  ],
  [
    '重新识别（key 变了）：编辑与排版结果作废，会话排版保留',
    'keep',
    [
      { type: 'layout', layout: 'join_lines', input: at() },
      { type: 'laid', key: 1, layout: 'join_lines', text: ONE },
      { type: 'edit', key: 1, text: '改过' },
    ],
    at(2, true, 'X\nY'),
    { layout: 'join_lines', display: 'X\nY', pending: true, edited: false, turned: false },
  ],
  [
    '迟到的上一次结果：丢弃',
    'remove_blank_lines',
    [
      { type: 'laid', key: 2, layout: 'remove_blank_lines', text: 'X' },
      { type: 'laid', key: 1, layout: 'remove_blank_lines', text: TIGHT },
    ],
    at(2, true, 'X\n\nX'),
    { display: 'X', pending: false },
  ],
  [
    '从固定交接还原、没有编辑：交接的文字就是排版结果，首帧不用再等',
    'keep',
    [{ type: 'restore', key: 3, layout: 'remove_blank_lines', edit: null, laid: TIGHT }],
    at(3),
    { layout: 'remove_blank_lines', display: TIGHT, pending: false },
  ],
  [
    '从固定交接还原、有编辑：显示编辑，排版结果另求',
    'keep',
    [{ type: 'restore', key: 3, layout: 'remove_blank_lines', edit: '改过', laid: null }],
    at(3),
    { display: '改过', pending: true, edited: true },
  ],
  [
    '从固定交接还原、交接时排版结果还没到：显示识别文本，排版结果另求（不把旧文字当成排版结果）',
    'keep',
    [{ type: 'restore', key: 3, layout: 'join_lines', edit: null, laid: null }],
    at(3),
    { layout: 'join_lines', display: RAW, pending: true, edited: false },
  ],
]

function run(initial: OcrLayout, actions: TextAction[]) {
  return actions.reduce(reduceText, initialText(initial))
}

describe('三层文字的状态表', () => {
  it.each(ROWS)('%s', (_name, initial, actions, input, expected) => {
    expect(viewOf(run(initial, actions), input)).toMatchObject(expected)
  })

  it('最近用过的整理方式：初值是去掉空行；设置默认不是原样时就是它；切到原样不改', () => {
    expect(initialText('keep').recent).toBe('remove_blank_lines')
    expect(initialText('join_lines').recent).toBe('join_lines')
    const s = run('keep', [
      { type: 'layout', layout: 'join_lines', input: at() },
      { type: 'layout', layout: 'keep', input: at() },
    ])
    expect(s.recent).toBe('join_lines')
  })

  it('编辑后又改回排版结果，再切换排版：编辑不保留，跟着新排版走', () => {
    const s = run('keep', [
      { type: 'edit', key: 1, text: RAW },
      { type: 'layout', layout: 'remove_blank_lines', input: at() },
      { type: 'laid', key: 1, layout: 'remove_blank_lines', text: TIGHT },
    ])
    expect(viewOf(s, at())).toMatchObject({ display: TIGHT, edit: null })
  })

  it('适用性：检视结果到之前都当作适用；到了以后按识别文本的可用变换；换了结果重新不知道', () => {
    let s = initialText('keep')
    expect(canApply(s, 1, 'remove_blank_lines')).toBe(true)
    s = reduceText(s, { type: 'inspected', key: 1, transforms: ['join_lines'] })
    expect(canApply(s, 1, 'keep')).toBe(true)
    expect(canApply(s, 1, 'join_lines')).toBe(true)
    expect(canApply(s, 1, 'remove_blank_lines')).toBe(false)
    expect(canApply(s, 2, 'remove_blank_lines')).toBe(true)
  })

  it('读屏播报只属于这次结果；切换时先清掉上一次的', () => {
    let s = reduceText(initialText('keep'), {
      type: 'notice',
      key: 0,
      layout: 'remove_blank_lines',
      copied: true,
    })
    expect(viewOf(s, at(0)).notice).toEqual({ layout: 'remove_blank_lines', copied: true })
    s = reduceText(s, { type: 'layout', layout: 'keep', input: at(0) })
    expect(viewOf(s, at(0)).notice).toBeNull()
    expect(viewOf(s, at(1)).notice).toBeNull()
  })
})
