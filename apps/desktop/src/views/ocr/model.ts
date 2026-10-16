/**
 * OCR 视图的纯函数：视觉模型列表、风格列表、请求构造、复制对象、面板内的高度分配。不碰 DOM / React，
 * 便于单测。
 */
import type {
  ModelEntry,
  ModelRef,
  OcrMode,
  Provider,
  SettingsSection,
  SettingsSnapshot,
  Style,
} from '../../shared/ipc'
import { isOcrService } from '../../shared/offline/catalog'
import { stripUrlBreaks } from '../../shared/stream/urlBreaks'

/** 三种识别模式（translate 是直接翻译，不在「重新识别」的模式列表里） */
export const OCR_MODES = ['lines', 'paragraphs', 'markdown'] as const satisfies readonly OcrMode[]

export interface VisionModel {
  provider: Provider
  model: ModelEntry
  ref: ModelRef
  /** vision = unknown：列出但带「未确认是否支持图片」 */
  unconfirmed: boolean
  /** 文字识别服务（PaddleOCR）的模型 */
  service: boolean
}

/**
 * 能用于 OCR 的模型：已启用、vision 为 yes / unknown（vision = no 的不出现）；文字识别服务（PaddleOCR）的模型按协议就能
 * 识别图片，不看 vision 字段
 */
export function visionModels(snapshot: SettingsSnapshot | null): VisionModel[] {
  const out: VisionModel[] = []
  for (const provider of snapshot?.settings.providers ?? []) {
    const service = isOcrService(provider)
    for (const model of provider.models) {
      if (!model.enabled || (!service && model.vision === 'no')) continue
      out.push({
        provider,
        model,
        ref: { provider_id: provider.id, model_id: model.id },
        unconfirmed: !service && model.vision === 'unknown',
        service,
      })
    }
  }
  return out
}

export const sameModel = (
  a: ModelRef | null | undefined,
  b: ModelRef | null | undefined,
): boolean => !!a && !!b && a.provider_id === b.provider_id && a.model_id === b.model_id

export const modelLabel = (m: ModelEntry): string => m.label || m.id

/** 风格列表（设置里的顺序；⌘1–9 依次对应前 9 个） */
export function styleList(snapshot: SettingsSnapshot | null): readonly Style[] {
  return snapshot?.styles ?? []
}

/** 默认风格：设置里的 default_style，找不到时取第一个，再退回 natural */
export function defaultStyleId(snapshot: SettingsSnapshot | null): string {
  const styles = styleList(snapshot)
  const wanted = snapshot?.settings.translate.default_style
  if (wanted && styles.some((s) => s.id === wanted)) return wanted
  return styles[0]?.id ?? wanted ?? 'natural'
}

/** 当前生效的快捷键（例如未授权时提示「授权后按 ⌥S 重新截图」）：组合串（给 Kbd）与显示串 */
export function hotkeyFor(
  snapshot: SettingsSnapshot | null,
  action: 'ocr' | 'ocr_translate',
): { combo: string | null; display: string } | null {
  const hit = snapshot?.runtime.hotkeys.find(
    (h) =>
      h.target.kind === 'action' &&
      h.target.action === action &&
      (h.state === 'registered' || h.state === 'fallback'),
  )
  return hit?.display ? { combo: hit.effective, display: hit.display } : null
}

/** 当前生效的快捷键显示串 */
export function hotkeyDisplay(
  snapshot: SettingsSnapshot | null,
  action: 'ocr' | 'ocr_translate',
): string | null {
  return hotkeyFor(snapshot, action)?.display ?? null
}

/** 未就绪 / 错误卡片的「去设置」应该打开哪个分组 */
export function settingsSectionFor(reason: string): SettingsSection {
  switch (reason) {
    case 'no_vision_model':
    case 'no_vision':
    case 'image_too_large':
      return 'ocr'
    case 'offline_model_missing':
    case 'offline_model_downloading':
    case 'offline_task_unsupported':
    case 'local_runtime':
      return 'offline'
    default:
      return 'providers'
  }
}

/**
 * ⌘C 的复制对象：先复制选区；没有选区时，译文区存在就复制译文，否则复制 OCR 文本（编辑后的）
 */
export function copyTarget(selection: string, translation: string | null, ocrText: string): string {
  if (selection.trim() !== '') return selection
  if (translation !== null && translation.trim() !== '') return translation
  return ocrText
}

/** 当前文档里的选中文字（输入框里的选区也算） */
export function selectedText(doc: Document = document): string {
  const el = doc.activeElement
  if (el instanceof HTMLTextAreaElement || el instanceof HTMLInputElement) {
    const { selectionStart: a, selectionEnd: b, value } = el
    if (a !== null && b !== null && b > a) return value.slice(a, b)
  }
  // 正文里 URL 的折行点（零宽空格，shared/stream/urlBreaks.ts）不进剪贴板
  return stripUrlBreaks(doc.getSelection()?.toString() ?? '')
}

/**
 * 面板内的版式常量（对齐视觉稿 design.ocr-*）：头部 = 缩略图 + 下边距；底栏 = 12 + 28；卡片上下内边距各 12。
 * 高度怎么分由 CSS flex 完成（ocr.css）：视图以卡片上限为 max-height（固定窗口铺满窗口），内容放不下时正文区先收缩
 * 到 `mainFloor`、再轮到展开区——同一次布局里完成，两区之和始终等于可用高度，不会先缩内容再动外框。
 */
export const LAYOUT = {
  /** 头部缩略图的高度上限；卡片上限低（< compactBelow）时用细一些的 48 px */
  thumb: 72,
  thumbCompact: 48,
  compactBelow: 440,
  /** 头部除缩略图外的高度（上下边距） */
  headExtra: 12,
  /** 直接翻译的语言条（36 + 下边距） */
  subhead: 40,
  footer: 40,
  pad: 24,
  /** 译文区 / 原文区的边框、内边距与头部 */
  section: 56,
  /** 区内底部状态行 */
  status: 30,
  /** 两区内容都放不下时，正文区（OCR 文本 / 直接翻译的译文）至少保留共享空间的这个比例（且不少于 3 行） */
  mainShare: 0.4,
} as const

/** 正文行高（`--text-body` × `--lh-body` 1.6；不取整，与排版里的实际行高一致） */
export const lineHeight = (fontSize: number): number => fontSize * 1.6

export interface LayoutInput {
  /** 卡片上限（ViewProps.maxHeight；固定窗口 = 窗口可用高度） */
  maxHeight: number
  fontSize: number
  /** 直接翻译（多一条语言条） */
  direct: boolean
}

export interface Layout {
  /** 视图内容的高度上限（卡片上限减去上下内边距） */
  viewMax: number
  /** 头部缩略图的高度上限 */
  thumb: number
  /** 内容放不下时正文区至少保留的高度（px）：共享空间的 40%，且不少于 3 行 */
  mainFloor: number
}

/**
 * 版式：卡片上限低时头部缩略图收成 48 px（把空间留给正文），正文区在两区都放不下时至少保留 40% / 3 行。展开区不设
 * 单独的上限：它按内容排版，正文区收缩到 mainFloor 之后它才开始收缩（flex-shrink 权重，见 ocr.css）。
 */
export function layoutFor(o: LayoutInput): Layout {
  const lh = lineHeight(o.fontSize)
  const thumb = o.maxHeight < LAYOUT.compactBelow ? LAYOUT.thumbCompact : LAYOUT.thumb
  const viewMax = Math.max(0, o.maxHeight - LAYOUT.pad)
  const shared =
    viewMax -
    thumb -
    LAYOUT.headExtra -
    (o.direct ? LAYOUT.subhead : 0) -
    LAYOUT.footer -
    LAYOUT.section -
    LAYOUT.status
  return { viewMax, thumb, mainFloor: Math.max(3 * lh, shared * LAYOUT.mainShare) }
}

/**
 * 缩略图的宽度上限（给尺寸 / 模式 · 模型两行与关闭按钮留位置）：窄面板（内容宽 < 400）取 38%，否则 55%，不超过
 * 240；内容宽度还没量到（0）时按 440 宽的卡片估计。Overlay 飞入的落点按同一规则（overlay/geometry.ts landingRect）
 */
export function thumbMaxWidth(contentWidth: number): number {
  const w = contentWidth > 0 ? contentWidth : 408
  return Math.max(96, Math.min(240, Math.round(w * (w < 400 ? 0.38 : 0.55))))
}

/**
 * 结构化结果里有没有需要预览的结构：管道表格（表头行 + 分隔行）、HTML 表格、独立成行的公式（`$$…$$` / `\[…\]`）。
 * 只做判定（面板首屏里的轻量检查）；真正的解析与渲染在懒加载的 structured/ 里
 */
export function hasStructure(text: string): boolean {
  return (
    /^\s*\|?(\s*:?-+:?\s*\|)+\s*:?-*:?\s*$/m.test(text) ||
    /<table[\s>]/i.test(text) ||
    /^\s*(\$\$|\\\[)/m.test(text)
  )
}
