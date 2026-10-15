/**
 * OCR 视图的公开出口（panel/views.ts 只从这里导入）。固定窗口（PinnedApp）渲染 OCR 内容时同样用 `OcrView`，
 * 并把 `pinned_init` 返回的 PinState 作为可选的 `pin` 属性传入（见 OcrViewProps）。
 */
export { OcrView, type OcrViewProps } from './OcrView'
