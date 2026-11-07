/**
 * 「展开阅读」的前端规则：固定窗口按自己的宽度决定版式；浏览器 mock 的几何与 Rust 的 `placement::reading_plan`
 * 同一套数值（宽度、最小工作区、宽截图、上限比例）。
 */
import { describe, expect, it } from 'vitest'
import { readingPlan } from '../../shared/ipc/mock/geometry'
import type { PanelPayload } from '../../shared/ipc/types'
import { isWideShot, pinnedReading, READING } from '../reading'

const translate: PanelPayload = {
  kind: 'translate',
  source_text: 'hello',
  request_id: null,
  task: 'translate',
  style_id: 'natural',
  detected_lang: 'en',
  target_lang: 'zh-CN',
  dict_mode: false,
  long_text: null,
  not_ready: null,
  selection_method: null,
}

function ocr(width: number, height: number): PanelPayload {
  return {
    kind: 'ocr',
    capture_id: 'c',
    request_id: null,
    mode: 'lines',
    thumb_url: 'crop/c.png',
    image_size: { width, height },
    target_lang: 'zh-CN',
    not_ready: null,
    layout: 'keep',
  }
}

const work = (width: number, height: number) => ({ x: 0, y: 0, width, height })

describe('宽截图', () => {
  it('逻辑宽高比达到 2 算宽截图；尺寸未知按高截图', () => {
    expect(isWideShot({ width: 400, height: 200 })).toBe(true)
    expect(isWideShot({ width: 399, height: 200 })).toBe(false)
    expect(isWideShot({ width: 0, height: 0 })).toBe(false)
    expect(isWideShot(null)).toBe(false)
  })
})

describe('固定窗口的阅读版式', () => {
  it('卡片宽度不到 680：普通布局', () => {
    expect(pinnedReading('translate', READING.minWidth - 1, { showSource: true })).toBeNull()
    expect(pinnedReading('ocr', 640, { shot: { width: 600, height: 900 } })).toBeNull()
  })
  it('翻译：开着原文才两栏', () => {
    expect(pinnedReading('translate', 760, { showSource: true })).toBe('columns')
    expect(pinnedReading('translate', 760, { showSource: false })).toBeNull()
  })
  it('OCR：高截图在卡片够宽时并排，宽截图或不够宽时堆叠', () => {
    const tall = { width: 600, height: 900 }
    expect(pinnedReading('ocr', 880, { shot: tall })).toBe('columns')
    expect(pinnedReading('ocr', READING.ocrColumnsWidth - 1, { shot: tall })).toBe('stacked')
    expect(pinnedReading('ocr', 880, { shot: { width: 1600, height: 200 } })).toBe('stacked')
  })
})

describe('mock 的展开阅读几何（与 placement::reading_plan 一致）', () => {
  it('翻译：工作区至少 680 宽时两栏 760（不超过工作区 − 16），上限 floor(工作区高 × 0.85)', () => {
    expect(readingPlan(translate, work(1512, 941), 520, 2)).toEqual({
      layout: 'columns',
      width: 760,
      maxHeight: 799,
    })
    expect(readingPlan(translate, work(700, 672), 520, 1.5)).toEqual({
      layout: 'columns',
      width: 684,
      maxHeight: 571,
    })
    expect(readingPlan(translate, work(679, 672), 520, 1)).toBeNull()
  })
  it('OCR：高截图并排 880；宽截图堆叠，宽度按截图 1:1（不窄于当前、不超过 880）；工作区不到 800 时高截图也堆叠', () => {
    expect(readingPlan(ocr(1200, 1800), work(1512, 941), 552, 2)).toEqual({
      layout: 'columns',
      width: 880,
      maxHeight: 799,
    })
    // 逻辑 700 × 100 → 宽 732
    expect(readingPlan(ocr(1400, 200), work(1512, 941), 552, 2)?.width).toBe(732)
    expect(readingPlan(ocr(1400, 200), work(1512, 941), 552, 2)?.layout).toBe('stacked')
    // 逻辑 300 × 60：不窄于当前宽度
    expect(readingPlan(ocr(600, 120), work(1512, 941), 552, 2)?.width).toBe(552)
    // 很宽的截图：不超过 880（框内横向滚动）
    expect(readingPlan(ocr(4000, 200), work(1512, 941), 552, 2)?.width).toBe(880)
    expect(readingPlan(ocr(600, 900), work(790, 600), 440, 1)?.layout).toBe('stacked')
  })
  it('其余视图没有阅读布局', () => {
    const input: PanelPayload = { kind: 'input', initial_text: '', target_lang: null, hint: null }
    expect(readingPlan(input, work(1512, 941), 420, 2)).toBeNull()
  })
})
