/** 提示条按优先级取舍 */
import { describe, expect, it } from 'vitest'
import { hiddenHints, type HintSize } from '../hints'

// 显示顺序：粘贴 · 纯文本 · 仅复制 · 置顶 · 删除 · 预览 · 操作 · 筛选 · 关闭；优先级同 ClipboardView 的 HintBar
const ORDER: ReadonlyArray<readonly [string, number]> = [
  ['paste', 0],
  ['plain', 1],
  ['copy', 4],
  ['pin', 6],
  ['delete', 5],
  ['preview', 7],
  ['actions', 8],
  ['filter', 2],
  ['close', 3],
]
const items = (w: Record<string, number>): HintSize[] =>
  ORDER.map(([id, priority]) => ({ id, priority, width: w[id] ?? 50 }))

describe('hiddenHints', () => {
  it('放得下时全部显示；没有排版（宽度为 0）时什么都不隐藏', () => {
    expect(hiddenHints(items({}), 1000, 10).size).toBe(0)
    expect(
      hiddenHints(
        items({}).map((i) => ({ ...i, width: 0 })),
        394,
        10,
      ).size,
    ).toBe(0)
    expect(hiddenHints(items({}), 0, 10).size).toBe(0)
  })

  it('窄宽度：必留 粘贴 / 纯文本 / 筛选 / 关闭，其余按 仅复制 → 删除 → 置顶 → 预览 → 操作 让位', () => {
    // 4 × 50 + 3 × 10 = 230；再放仅复制 290、删除 350；置顶要 410 > 394
    const hidden = hiddenHints(items({}), 394, 10)
    expect([...hidden].sort()).toEqual(['actions', 'pin', 'preview'])
  })

  it('严格按优先级：前一项放不下之后，更短的低优先级项也不显示', () => {
    // 仅复制很宽放不下，删除虽然很窄也不再显示
    const hidden = hiddenHints(items({ copy: 300, delete: 10 }), 394, 10)
    expect(hidden.has('copy')).toBe(true)
    expect(hidden.has('delete')).toBe(true)
    expect(hidden.has('close')).toBe(false)
  })

  it('连必留项都放不下时从最低优先级的必留项起隐藏（不会出现半截提示）', () => {
    const hidden = hiddenHints(items({ paste: 120, plain: 150, filter: 100, close: 90 }), 394, 10)
    expect(hidden.has('paste')).toBe(false)
    expect(hidden.has('plain')).toBe(false)
    expect(hidden.has('filter')).toBe(false)
    expect(hidden.has('close')).toBe(true)
  })
})
