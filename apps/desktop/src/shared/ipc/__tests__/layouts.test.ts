/**
 * OCR 排版规则的契约：`layouts.ts` 与 Rust 的 `OcrLayout::ALL` / `can_be_default` / `available_in` 逐项一致。样例由
 * `UPDATE_GOLDEN=1 cargo test -p wisp-types --test consts`（或 `node scripts/gen-fixtures.mjs`）生成。
 */
import { describe, expect, it } from 'vitest'
import rust from '../fixtures/consts/OcrLayouts.json'
import { canBeDefaultLayout, layoutAvailableIn, OCR_LAYOUTS } from '../layouts'
import type { OcrLayout, OcrMode } from '../types'
import { registry, resolve } from './shapes'

const RULES = rust as Record<OcrLayout, { can_be_default: boolean; available_in: OcrMode[] }>

function enumValues(name: 'OcrLayout' | 'OcrMode'): string[] {
  const s = resolve(registry[name])
  if (s.t !== 'enum') throw new Error(`期望枚举，实际是 ${s.t}`)
  return [...s.values]
}

describe('OCR 排版规则（Rust ↔ TS）', () => {
  it('OCR_LAYOUTS 与 Rust 的 OcrLayout::ALL 逐项一致（含顺序），也覆盖 types.ts 的联合类型', () => {
    expect([...OCR_LAYOUTS]).toEqual(Object.keys(RULES))
    expect([...OCR_LAYOUTS].sort()).toEqual(enumValues('OcrLayout').sort())
  })

  it('能否作为默认与 can_be_default 一致', () => {
    for (const layout of OCR_LAYOUTS) {
      expect(canBeDefaultLayout(layout), layout).toBe(RULES[layout].can_be_default)
    }
  })

  it('各模式下是否可用与 available_in 一致', () => {
    const modes = enumValues('OcrMode') as OcrMode[]
    for (const layout of OCR_LAYOUTS) {
      const available = modes.filter((m) => layoutAvailableIn(layout, m))
      expect(available.sort(), layout).toEqual([...RULES[layout].available_in].sort())
    }
    expect(layoutAvailableIn('reflow', 'paragraphs')).toBe(false)
  })
})
