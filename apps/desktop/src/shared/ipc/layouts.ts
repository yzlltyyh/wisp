/**
 * OCR 结果排版的规则：哪些排版能作为设置里的默认、每种排版在哪些识别模式下可用。
 *
 * 与 Rust 的 `OcrLayout::can_be_default` / `OcrLayout::available_in` 逐项一致：`fixtures/consts/OcrLayouts.json` 由 Rust
 * 生成（`node scripts/gen-fixtures.mjs`），`__tests__/layouts.test.ts` 比对两边。设置页只列可作默认的排版，结果面板只列
 * 当前模式可用的排版；编排器交付时也按同一条规则决定是否排版（不可用时原样交付），所以视图显示的文字与自动复制的一致。
 */
import type { OcrLayout, OcrMode } from './types'

/** 全部排版，顺序与 Rust 的 `OcrLayout::ALL` 相同（从改动少到改动多） */
export const OCR_LAYOUTS = [
  'keep',
  'remove_blank_lines',
  'reflow',
  'join_lines',
] as const satisfies readonly OcrLayout[]

/** 能作为设置里默认排版的取值：不改动行结构的原样与去掉空行 */
export type DefaultOcrLayout = Extract<OcrLayout, 'keep' | 'remove_blank_lines'>

/** 能否作为设置里的默认排版（合并段内换行、合并为一行只能在结果面板里按需切换） */
export function canBeDefaultLayout(layout: OcrLayout): layout is DefaultOcrLayout {
  return layout === 'keep' || layout === 'remove_blank_lines'
}

/**
 * 这种排版在某个识别模式下是否可用：原样总是可用；去掉空行与合并为一行只在逐行与段落模式；合并段内换行只在逐行模式
 * （段落模式的结果已经按段合并过）
 */
export function layoutAvailableIn(layout: OcrLayout, mode: OcrMode): boolean {
  switch (layout) {
    case 'keep':
      return true
    case 'reflow':
      return mode === 'lines'
    case 'remove_blank_lines':
    case 'join_lines':
      return mode === 'lines' || mode === 'paragraphs'
  }
}
