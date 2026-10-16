/**
 * 离线模型与文字识别服务的纯函数：模型 id 的分类与顺序、离线模型在「模型引用」里的写法、各识别引擎支持的模式、
 * 下载状态的进度与剩余时间、引导页的两档组合。设置、引导与面板共用，不依赖 React，便于单测。
 *
 * 离线模型不写进 `defaults` 与风格（它们会同步到其他设备）：选中的离线模型只在本机的 `offline.*_model` 里。
 * 面板「重新识别 ▾」这类一次性的指定用保留的服务商 id `@offline` 拼成模型引用，`model_id` 就是离线模型的 id。
 */
import type {
  FitnessLevel,
  LocalModelId,
  LocalModelInfo,
  LocalModelKind,
  LocalModelState,
  LocalRuntimeId,
  LocalRuntimeInfo,
  ModelRef,
  OcrMode,
  Protocol,
  Provider,
} from '../ipc/types'

/** 离线模型的保留服务商 id（与 Rust 的 `defaults::OFFLINE_PROVIDER_ID` 一致；真实服务商的 id 不能以 `@` 开头） */
export const OFFLINE_PROVIDER_ID = '@offline'

/** 文字识别类，按界面里的顺序（从快到强） */
export const OCR_MODELS = [
  'pp_ocrv6_tiny',
  'pp_ocrv6_small',
  'paddleocr_vl_1_6',
] as const satisfies readonly LocalModelId[]
/** 翻译类，按界面里的顺序 */
export const TRANSLATE_MODELS = [
  'index_translate_2b',
  'index_translate_9b',
] as const satisfies readonly LocalModelId[]
export const LOCAL_MODELS: readonly LocalModelId[] = [...OCR_MODELS, ...TRANSLATE_MODELS]

export function isLocalModelId(value: unknown): value is LocalModelId {
  return typeof value === 'string' && (LOCAL_MODELS as readonly string[]).includes(value)
}

/**
 * 各模型依赖的推理组件（与 Rust 的模型目录一致）：PP-OCRv6 用 ONNX Runtime；PaddleOCR-VL 用 llama.cpp 识别、ONNX Runtime
 * 做版面检测；Index-Translate 用 llama.cpp。只用于估算「一共要下载多少」，以 Rust 快照里的数字为准
 */
export const MODEL_RUNTIMES: Record<LocalModelId, readonly LocalRuntimeId[]> = {
  pp_ocrv6_tiny: ['onnx_runtime'],
  pp_ocrv6_small: ['onnx_runtime'],
  paddleocr_vl_1_6: ['llama_cpp', 'onnx_runtime'],
  index_translate_2b: ['llama_cpp'],
  index_translate_9b: ['llama_cpp'],
}

export function modelKind(id: LocalModelId): LocalModelKind {
  return (OCR_MODELS as readonly string[]).includes(id) ? 'ocr' : 'translate'
}

/** 一次性指定离线模型时的模型引用 */
export function offlineRef(id: LocalModelId): ModelRef {
  return { provider_id: OFFLINE_PROVIDER_ID, model_id: id }
}

/** 模型引用指向离线模型时返回它的 id，否则 null */
export function offlineIdOf(
  ref: Pick<ModelRef, 'provider_id' | 'model_id'> | null,
): LocalModelId | null {
  if (!ref || ref.provider_id !== OFFLINE_PROVIDER_ID) return null
  return isLocalModelId(ref.model_id) ? ref.model_id : null
}

/** 文字识别服务（不是 LLM：只能用于 OCR，不能翻译，也没有「获取模型列表」） */
export function isOcrService(p: Pick<Provider, 'protocol'> | { protocol: Protocol }): boolean {
  return p.protocol === 'paddle_ocr'
}

/**
 * PaddleOCR 云端的模型是否为文档解析类（输出 Markdown：PP-StructureV3、PaddleOCR-VL 系列）；PP-OCRv6 / v5 只输出文本行。
 * 与 `wisp-local` 的 `ocr::cloud::is_document_model` 同一规则
 */
export function isDocumentModel(model: string): boolean {
  const m = model.trim().toLowerCase()
  return m.startsWith('pp-structure') || m.startsWith('paddleocr-vl')
}

/** 明显较慢的云端模型（PP-StructureV3 服务端约半分钟一页）：服务商页给出提示 */
export function isSlowCloudModel(model: string): boolean {
  return model.trim().toLowerCase().startsWith('pp-structure')
}

/** 一次识别实际用到的引擎（决定支持哪些模式） */
export type OcrEngine =
  { kind: 'llm' } | { kind: 'offline'; model: LocalModelId } | { kind: 'cloud'; model: string }

/**
 * 当前生效的识别引擎：一次性指定 > 本机选择的离线模型 > 默认 OCR 模型。指向的服务不存在时按 LLM 处理（模式不受限，
 * 真正的问题由 Rust 的未就绪卡片说明）
 */
export function ocrEngineOf(
  explicit: ModelRef | null,
  offline: LocalModelId | null,
  defaults: ModelRef | null,
  providers: readonly Pick<Provider, 'id' | 'protocol'>[],
): OcrEngine {
  const pick = (ref: ModelRef | null): OcrEngine | null => {
    if (!ref) return null
    const local = offlineIdOf(ref)
    if (local) return { kind: 'offline', model: local }
    const p = providers.find((x) => x.id === ref.provider_id)
    if (p && isOcrService(p)) return { kind: 'cloud', model: ref.model_id }
    return { kind: 'llm' }
  }
  return (
    pick(explicit) ??
    (offline ? { kind: 'offline', model: offline } : null) ??
    pick(defaults) ?? { kind: 'llm' }
  )
}

/**
 * 引擎是否支持某个识别模式：PP-OCRv6（本地）与云端的 PP-OCRv6 / v5 只识别文本行，没有版面与表格，不支持「结构化」；
 * 其余都支持。直接翻译（translate）总是可用（不能一步完成的引擎由 Rust 先识别再翻译）
 */
export function engineSupports(engine: OcrEngine, mode: OcrMode): boolean {
  if (mode !== 'markdown') return true
  if (engine.kind === 'offline') return engine.model === 'paddleocr_vl_1_6'
  if (engine.kind === 'cloud') return isDocumentModel(engine.model)
  return true
}

// ---------------------------------------------------------------------------
// 下载状态

/** 正在进行的下载（排队、下载中、校验中）：不能再点下载，可以暂停 / 取消 */
export function isActive(state: LocalModelState): boolean {
  return state.state === 'queued' || state.state === 'downloading' || state.state === 'verifying'
}

/** 0–1 的进度；没有确定进度（排队、校验中、未开始）时为 null */
export function progressOf(state: LocalModelState): number | null {
  if (state.state !== 'downloading' && state.state !== 'paused') return null
  if (state.total <= 0) return 0
  return Math.max(0, Math.min(1, state.done / state.total))
}

/**
 * 剩余秒数：速度为 0（刚开始、断流）或很慢到不可信（超过一天）时不给出，避免「剩余 9999 小时」
 */
export function etaSeconds(state: LocalModelState): number | null {
  if (state.state !== 'downloading' || state.bytes_per_sec <= 0) return null
  const left = Math.max(0, state.total - state.done)
  const s = left / state.bytes_per_sec
  return s > DAY_S ? null : Math.ceil(s)
}
const DAY_S = 24 * 60 * 60

/** 还需要下载的字节数（模型 + 一并下载的推理组件） */
export function downloadSize(
  m: Pick<LocalModelInfo, 'download_bytes' | 'pending_runtime_bytes'>,
): number {
  return m.download_bytes + m.pending_runtime_bytes
}

/**
 * 点下载之前要先确认的设备适配：内存不足、只能在 CPU 上慢慢跑，以及还没有结论（Windows / Linux 要先装好推理组件才能
 * 检测显卡；没有独立显卡时 9B / VL 会很慢，下载量又是 GB 级，先让用户知道再下）
 */
export function needsFitConfirm(level: FitnessLevel): boolean {
  return level !== 'good'
}

/**
 * 已暂停的下载点「继续」之前要先确认的适配：检测出较慢 / 内存不足。显卡检测发生在推理组件装好之后，检测结果不理想时
 * 下载会自动暂停在这里，由用户决定是否继续下载模型本身
 */
export function needsResumeConfirm(level: FitnessLevel): boolean {
  return level === 'insufficient' || level === 'slow'
}

/** 依赖 llama.cpp 的模型（第一次加载时要编译显卡着色器，比 PP-OCRv6 慢得多） */
export function usesLlama(id: LocalModelId): boolean {
  return MODEL_RUNTIMES[id].includes('llama_cpp')
}

/**
 * 模型此刻能不能直接用：模型文件已安装，且它需要的推理组件也都装好了（推理组件被单独删掉、或者没装完时，模型虽然是
 * 「已下载」，请求仍然会被判为未就绪；这时再下载一次只会补齐推理组件）
 */
export function isUsable(m: Pick<LocalModelInfo, 'state' | 'pending_runtime_bytes'>): boolean {
  return m.state.state === 'installed' && m.pending_runtime_bytes === 0
}

// ---------------------------------------------------------------------------
// 引导页的两档组合

export type OfflinePack = 'light' | 'quality'

export const PACKS: Record<OfflinePack, { ocr: LocalModelId; translate: LocalModelId }> = {
  light: { ocr: 'pp_ocrv6_small', translate: 'index_translate_2b' },
  quality: { ocr: 'paddleocr_vl_1_6', translate: 'index_translate_9b' },
}

const FIT_RANK: Record<FitnessLevel, number> = { good: 0, unknown: 1, slow: 2, insufficient: 3 }

/** 一档组合里较差的那个适配（决定这一档是否推荐） */
export function packFitness(
  pack: OfflinePack,
  models: readonly Pick<LocalModelInfo, 'id' | 'fitness'>[],
): FitnessLevel {
  const { ocr, translate } = PACKS[pack]
  let worst: FitnessLevel = 'good'
  for (const id of [ocr, translate]) {
    const level = models.find((m) => m.id === id)?.fitness.level ?? 'unknown'
    if (FIT_RANK[level] > FIT_RANK[worst]) worst = level
  }
  return worst
}

/** 推荐哪一档：高质量这一档完全适合本机时推荐它，否则推荐轻量 */
export function recommendedPack(
  models: readonly Pick<LocalModelInfo, 'id' | 'fitness'>[],
): OfflinePack {
  return packFitness('quality', models) === 'good' ? 'quality' : 'light'
}

/** 一档组合需要下载的总量（已安装的不计；两个模型共用的推理组件只算一次） */
export function packSize(
  pack: OfflinePack,
  models: readonly Pick<LocalModelInfo, 'id' | 'download_bytes' | 'state'>[],
  runtimes: readonly Pick<LocalRuntimeInfo, 'id' | 'installed' | 'size'>[],
): number {
  let total = 0
  const needed = new Set<LocalRuntimeId>()
  for (const id of [PACKS[pack].ocr, PACKS[pack].translate]) {
    const m = models.find((x) => x.id === id)
    if (!m || m.state.state === 'installed') continue
    total += m.download_bytes
    for (const r of MODEL_RUNTIMES[id]) needed.add(r)
  }
  for (const r of runtimes) if (needed.has(r.id) && !r.installed) total += r.size
  return total
}
