/**
 * mock 的「粘贴为…」变换只查 Rust 生成的样例表（`fixtures/consts/TransformCases.json`），不自己实现规则：表里的文字
 * 逐字给出 Rust 的结果，表外的文字原样返回并提示。
 */
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import * as api from '../../commands'
import TRANSFORM_CASES from '../../fixtures/consts/TransformCases.json'
import rustTransformIds from '../../fixtures/consts/TransformIds.json'
import { whenReady } from '../../runtime'
import type { TransformId } from '../../types'
import { mockInspect, mockTransform } from '../smart'

const CASES = TRANSFORM_CASES as {
  text: string
  transforms: Partial<Record<TransformId, string>>
}[]
const IDS = rustTransformIds as TransformId[]

beforeAll(async () => {
  await new Promise<void>((resolve) => whenReady(resolve))
  // mock 安装后在后台加载全部场景模块（HUD 的场景列表）：等它们加载完，用例很快结束时环境不会在加载途中被拆掉
  await window.__WISP_MOCK__?.listScenarios()
})

afterEach(() => {
  vi.restoreAllMocks()
})

describe('mock 的文本变换（Rust 样例表）', () => {
  it('样例表覆盖每一种变换，列出的变换按菜单顺序', () => {
    expect(CASES.length).toBeGreaterThan(0)
    expect(IDS.filter((id) => CASES.some((c) => id in c.transforms))).toEqual(IDS)
    for (const c of CASES) {
      const order = Object.keys(c.transforms).map((id) => IDS.indexOf(id as TransformId))
      expect(order.every((i) => i >= 0)).toBe(true)
      expect(order).toEqual([...order].sort((a, b) => a - b))
    }
  })

  it('表里的文字：结果与 Rust 逐字相同，不适用时原样返回', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    for (const c of CASES) {
      for (const id of IDS) expect(mockTransform(c.text, id)).toBe(c.transforms[id] ?? c.text)
    }
    expect(warn).not.toHaveBeenCalled()
  })

  it('表里的文字：「粘贴为…」列出的变换就是 Rust 检视的结果（文本与富文本）', () => {
    for (const c of CASES) {
      expect(mockInspect('text', c.text).transforms).toEqual(Object.keys(c.transforms))
      expect(mockInspect('rich_text', c.text).transforms).toEqual(Object.keys(c.transforms))
    }
  })

  it('表外的文字原样返回并提示（提示里不带内容）', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const text = '不在样例表里的一段文字\n\n第二段'
    expect(mockTransform(text, 'remove_blank_lines')).toBe(text)
    expect(warn).toHaveBeenCalledTimes(1)
    expect(String(warn.mock.calls[0]?.[0])).not.toContain('第二段')
  })

  it('text_transform 命令经 mock 传输层查表', async () => {
    const c = CASES.find((x) => x.transforms.remove_blank_lines !== undefined)
    if (!c) throw new Error('样例表里应有适用「去掉空行」的文字')
    await expect(api.textTransform(c.text, 'remove_blank_lines')).resolves.toBe(
      c.transforms.remove_blank_lines,
    )
    await expect(api.textTransform(c.text, 'json_minify')).resolves.toBe(c.text)
  })
})
