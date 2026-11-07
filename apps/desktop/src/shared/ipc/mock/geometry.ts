/**
 * Rust 面板几何规则的 mock 镜像（只在浏览器 mock 里用）。数值与 `windows/placement.rs` 一致，改了那边要同步这里。
 */
import type { PanelPayload, ReadingLayout, Rect } from '../types'

/** 面板与工作区边缘的最小距离 */
const EDGE_INSET = 8
/** 卡片左右内边距之和 */
const CONTENT_INSET = 32
/** 「展开阅读」（`placement::reading_plan`）：翻译两栏宽、OCR 并排宽，各自要求的最小工作区宽度，宽截图的宽高比、上限比例 */
const READING_TRANSLATE_WIDTH = 760
const READING_OCR_WIDTH = 880
const READING_TRANSLATE_MIN_WORK = 680
const READING_OCR_MIN_WORK = 800
const READING_WIDE_ASPECT = 2
const READING_HEIGHT_RATIO = 0.85

export interface ReadingPlan {
  layout: ReadingLayout
  width: number
  /** 卡片上限（拖过的面板：会话上限与整个工作区高度取较小者） */
  maxHeight: number
}

/**
 * `placement::reading_plan` + 「拖过的面板」的上限：翻译工作区够宽时两栏 760；OCR 高截图在工作区够宽时并排 880，
 * 否则上下堆叠（宽度按截图 1:1，不窄于当前宽度、不超过 880）。截图尺寸按 `scale` 从物理像素换算。输入视图按翻译算
 * （提交后在同一会话里换成翻译视图，`placement::reading_subject`）
 */
export function readingPlan(
  payload: PanelPayload,
  work: Rect,
  currentWidth: number,
  scale = typeof devicePixelRatio === 'number' && devicePixelRatio > 0 ? devicePixelRatio : 1,
): ReadingPlan | null {
  const fit = (w: number) => Math.round(Math.min(w, work.width - 2 * EDGE_INSET))
  const cap = Math.floor(work.height * READING_HEIGHT_RATIO)
  const maxHeight = Math.floor(Math.min(cap, work.height - 2 * EDGE_INSET))
  if (payload.kind === 'translate' || payload.kind === 'input') {
    return work.width >= READING_TRANSLATE_MIN_WORK
      ? { layout: 'columns', width: fit(READING_TRANSLATE_WIDTH), maxHeight }
      : null
  }
  if (payload.kind !== 'ocr') return null
  const { width: pw, height: ph } = payload.image_size
  const shot = pw > 0 && ph > 0 ? { w: pw / scale, h: ph / scale } : null
  const wide = !!shot && shot.w >= shot.h * READING_WIDE_ASPECT
  if (!wide && work.width >= READING_OCR_MIN_WORK)
    return { layout: 'columns', width: fit(READING_OCR_WIDTH), maxHeight }
  const natural = shot ? Math.round(shot.w + CONTENT_INSET) : currentWidth
  return {
    layout: 'stacked',
    width: fit(Math.max(currentWidth, Math.min(natural, READING_OCR_WIDTH))),
    maxHeight,
  }
}

/** OCR 视图的默认宽度（框选较窄、光标锚点与未就绪时） */
export const OCR_WIDTH = 440
/** OCR 视图随框选变宽的上限 */
export const OCR_MAX_WIDTH = 600

/**
 * `placement::ocr_width`：OCR 卡片宽度随框选，`clamp(round(框选宽 × 字号 / 14 + 32), 440, 600)`——框选里的一行按设置
 * 字号重排后放得下、逐行结果不折行。框选宽度不是有限正数时取 440
 */
export function ocrPanelWidth(captureWidth: number, fontSize = 14): number {
  if (!(Number.isFinite(captureWidth) && captureWidth > 0)) return OCR_WIDTH
  const font = Number.isFinite(fontSize) && fontSize > 0 ? fontSize : 14
  return Math.min(OCR_MAX_WIDTH, Math.max(OCR_WIDTH, Math.round((captureWidth * font) / 14 + 32)))
}
