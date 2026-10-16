/**
 * 固定窗口的交接（kind = ocr 的 PinState，字段约定见 OcrViewProps.pin）：面板「固定」时由视图当前的状态组装，
 * 固定窗口打开时据它还原。这里只做数据转换，可以单独测试；接入请求、写入各区与 React 状态仍由 OcrView 完成。
 */
import { formatDuration } from '../../shared/i18n/format'
import type {
  Lang,
  OcrLayers,
  OcrLayout,
  OcrMode,
  OcrPayload,
  PinState,
} from '../../shared/ipc/types'
import { pinStatusOf, seedOf, type Run, type RunState } from './useRun'

/** 组装时只读每个区的状态与全文（测试不必构造完整的 Run） */
export type RunView = Pick<Run, 'state' | 'text'>

export interface PinInput {
  /** 直接翻译（`payload.mode = translate`）：正文区是译文，「原文」是按需的转录 */
  direct: boolean
  payload: Pick<OcrPayload, 'capture_id' | 'target_lang' | 'thumb_url' | 'image_size'>
  /** 正文区（识别；直接翻译时是译文） */
  main: RunView
  /** 译文区（OCR 模式） */
  tr: RunView
  /** 原文转录（直接翻译） */
  src: RunView
  mode: OcrMode
  /** 当前显示的 OCR 文本（编辑过的以编辑后的为准，否则是排版后的） */
  ocrText: string
  /** 结果文字的三层（识别文本、会话排版、编辑）；直接翻译为 null */
  layers: OcrLayers | null
  /** 当前生效的风格 */
  style: string
  /** 用户换过的目标语言；null = 没换过 */
  target: Lang | null
  trOpen: boolean
  srcOpen: boolean
}

const settled = (s: RunState): boolean => s.phase === 'done' || s.phase === 'cancelled'

/** 一行写完的状态（模型 · 耗时）：只留给读不懂结构化 `status` 的旧前端 */
export function statusLineOf(s: RunState): string | null {
  const parts = [
    s.start?.model_label,
    s.done && !s.done.cached ? formatDuration(s.done.elapsed_ms) : null,
  ].filter(Boolean)
  return parts.length ? parts.join(' · ') : null
}

/** 固定交接的共同字段：正文区的结构化状态行、实际模型、缩略图与原图尺寸 */
function pinExtras(
  s: RunState,
  payload: PinInput['payload'],
): Pick<PinState, 'status' | 'model_ref' | 'thumb_url' | 'image_size'> {
  const st = s.start
  return {
    status: pinStatusOf(s),
    model_ref: st ? { provider_id: st.provider_id, model_id: st.model_id } : null,
    thumb_url: payload.thumb_url || null,
    image_size:
      payload.image_size.width > 0 && payload.image_size.height > 0 ? payload.image_size : null,
  }
}

/** 面板「固定」时交给 `panel_pin` 的状态 */
export function pinStateOf(input: PinInput): PinState {
  const { direct, payload, main, tr, src, mode, ocrText, layers, style, target, trOpen, srcOpen } =
    input
  if (direct) {
    return {
      kind: 'ocr',
      source_text: src.state.hasText ? src.text() : '',
      result_text: main.text(),
      task: 'translate',
      style_id: style,
      target_lang: main.state.meta?.target_lang ?? target ?? payload.target_lang,
      detected_lang: main.state.meta?.detected_source_lang ?? null,
      request_id: main.state.requestId,
      capture_id: payload.capture_id,
      ocr_text: src.state.phase === 'done' ? src.text() : null,
      ocr_mode: 'translate',
      show_source: srcOpen,
      status_line: statusLineOf(main.state),
      ...pinExtras(main.state, payload),
    }
  }
  const ocrDone = settled(main.state)
  // 译文区展开但译文请求还没有 id（被拒绝、或还没回 ack）：固定窗口不能把识别请求接到译文区上
  const trAttach = trOpen && tr.state.requestId !== null
  return {
    kind: 'ocr',
    source_text: ocrDone ? ocrText : '',
    result_text: trAttach ? tr.text() : '',
    task: 'translate',
    style_id: style,
    target_lang: tr.state.meta?.target_lang ?? target ?? payload.target_lang,
    detected_lang: tr.state.meta?.detected_source_lang ?? null,
    request_id: ocrDone && trAttach ? tr.state.requestId : main.state.requestId,
    capture_id: payload.capture_id,
    ocr_text: ocrDone ? ocrText : null,
    ocr_mode: mode,
    show_source: ocrDone && trAttach,
    status_line: statusLineOf(main.state),
    ...pinExtras(main.state, payload),
    // 还在识别时只交接会话排版（固定窗口识别完成后按它排版），文字等识别结果
    ...(layers
      ? { ocr_layers: ocrDone ? layers : { raw: '', layout: layers.layout, edit: null } }
      : null),
  }
}

/** 译文所依据的原文：识别文本 + 编辑（切换排版不算原文改变）、风格与目标语言 */
export interface TranslationBasis {
  raw: string
  /** 有编辑时的文字；没有编辑为 null */
  edit: string | null
  style: string
  target: Lang | null
}

/** 还原一个区的内容 */
export interface RunSeed {
  /** 要接入的请求（流已结束也接，回放里有模型 / 耗时 / 用量）；null = 直接用 `text` 置为结束 */
  attach: string | null
  /** 置为结束时的文字；接入失败（日志已释放）时同样退回它 */
  text: string
  extra: Partial<RunState>
  phase: 'done' | 'cancelled'
}

export interface PinRestore {
  /** 正文区（识别；直接翻译时是译文） */
  main: RunSeed
  /** 直接翻译：已有的原文转录（null = 固定前还没有转录完）与「原文」是否展开；OCR 模式为 null */
  source: { text: string | null; open: boolean } | null
  /** OCR 模式且译文区展开：译文区的内容，以及译文所依据的原文与风格；否则为 null */
  translation: (RunSeed & { basis: TranslationBasis }) | null
  /**
   * OCR 模式且识别已结束：正文的排版与编辑（`shown` 是交接时显示的文字，没有编辑时即排版结果）；正文区的识别文本在
   * `main.text` 里。旧面板没有交接三层时按「原样、没有编辑」还原
   */
  text: { layout: OcrLayout; edit: string | null; shown: string } | null
}

/** 固定窗口打开时怎么还原各区 */
export function pinRestoreOf(p: PinState, direct: boolean): PinRestore {
  // 正文区的模型 / 耗时 / 用量来自结构化状态行；固定前已取消的仍是已取消
  const restored = seedOf(p.status ?? null, p.model_ref ?? null)
  const phase = p.status?.cancelled ? 'cancelled' : 'done'
  const main = (text: string, attach: string | null): RunSeed => ({
    attach,
    text,
    extra: restored,
    phase,
  })
  if (direct) {
    return {
      main: main(p.result_text, p.request_id),
      source: { text: p.ocr_text, open: p.show_source && p.ocr_text !== null },
      translation: null,
      text: null,
    }
  }
  // 还在识别：接上识别请求
  if (p.ocr_text === null) {
    return { main: main(p.source_text, p.request_id), source: null, translation: null, text: null }
  }
  // 识别已结束：正文区是识别文本（旧面板只交接了显示的文字，就当它是识别文本），排版与编辑另行还原；模型 / 耗时 /
  // 用量取结构化状态行，头部与状态行与面板一致
  const layers = p.ocr_layers ?? null
  const raw = layers?.raw ?? p.ocr_text
  const edit = layers?.edit ?? null
  return {
    main: main(raw, null),
    source: null,
    translation: p.show_source
      ? {
          attach: p.request_id,
          text: p.result_text,
          extra: {},
          phase: 'done',
          basis: { raw, edit, style: p.style_id, target: null },
        }
      : null,
    text: { layout: layers?.layout ?? 'keep', edit, shown: p.ocr_text },
  }
}
