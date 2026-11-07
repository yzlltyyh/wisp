/**
 * Rust 面板几何规则的 mock 镜像（只在浏览器 mock 里用）。数值与 `windows/placement.rs` 一致，改了那边要同步这里。
 */

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
