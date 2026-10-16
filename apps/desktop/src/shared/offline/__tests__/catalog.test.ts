/**
 * 离线模型的纯函数：模型引用的写法、识别引擎与支持的模式、下载进度与剩余时间、引导页两档组合的推荐与总量。
 */
import { describe, expect, it } from 'vitest'
import { LocalModelsSnapshot as Golden } from '../../ipc/fixtures'
import type { LocalModelInfo, LocalModelsSnapshot, Provider } from '../../ipc/types'
import {
  engineSupports,
  etaSeconds,
  isDocumentModel,
  isOcrService,
  isUsable,
  modelKind,
  needsFitConfirm,
  needsResumeConfirm,
  ocrEngineOf,
  offlineIdOf,
  offlineRef,
  packFitness,
  packSize,
  progressOf,
  recommendedPack,
} from '../catalog'

const snap = (): LocalModelsSnapshot => structuredClone(Golden) as LocalModelsSnapshot
const providers: Pick<Provider, 'id' | 'protocol'>[] = [
  { id: 'openai', protocol: 'openai_responses' },
  { id: 'paddle', protocol: 'paddle_ocr' },
]

describe('模型引用与分类', () => {
  it('离线模型用保留的服务商 id 拼成模型引用；其他引用不是离线模型', () => {
    expect(offlineRef('pp_ocrv6_small')).toEqual({
      provider_id: '@offline',
      model_id: 'pp_ocrv6_small',
    })
    expect(offlineIdOf(offlineRef('index_translate_9b'))).toBe('index_translate_9b')
    expect(offlineIdOf({ provider_id: '@offline', model_id: 'gpt-5' })).toBeNull()
    expect(offlineIdOf({ provider_id: 'openai', model_id: 'pp_ocrv6_small' })).toBeNull()
    expect(offlineIdOf(null)).toBeNull()
    expect(modelKind('paddleocr_vl_1_6')).toBe('ocr')
    expect(modelKind('index_translate_2b')).toBe('translate')
  })

  it('PaddleOCR 是文字识别服务；文档解析类模型与 Rust 的规则一致', () => {
    expect(isOcrService({ protocol: 'paddle_ocr' })).toBe(true)
    expect(isOcrService({ protocol: 'openai_chat' })).toBe(false)
    expect(isDocumentModel('PP-StructureV3')).toBe(true)
    expect(isDocumentModel(' PaddleOCR-VL-1.6 ')).toBe(true)
    expect(isDocumentModel('PP-OCRv6')).toBe(false)
    expect(isDocumentModel('PP-OCRv5')).toBe(false)
  })
})

describe('识别引擎与模式', () => {
  it('优先级：一次性指定 > 本机的离线模型 > 默认 OCR 模型', () => {
    const defaults = { provider_id: 'openai', model_id: 'gpt-5.4-mini' }
    expect(ocrEngineOf(null, null, defaults, providers)).toEqual({ kind: 'llm' })
    expect(ocrEngineOf(null, 'pp_ocrv6_tiny', defaults, providers)).toEqual({
      kind: 'offline',
      model: 'pp_ocrv6_tiny',
    })
    expect(
      ocrEngineOf(
        { provider_id: 'paddle', model_id: 'PP-OCRv6' },
        'pp_ocrv6_tiny',
        defaults,
        providers,
      ),
    ).toEqual({ kind: 'cloud', model: 'PP-OCRv6' })
    expect(ocrEngineOf(null, null, { provider_id: 'gone', model_id: 'x' }, providers)).toEqual({
      kind: 'llm',
    })
  })

  it('PP-OCRv6（本地与云端）不支持「结构化」；VL、文档解析类与 LLM 都支持；其他模式总是可用', () => {
    expect(engineSupports({ kind: 'offline', model: 'pp_ocrv6_small' }, 'markdown')).toBe(false)
    expect(engineSupports({ kind: 'offline', model: 'pp_ocrv6_small' }, 'paragraphs')).toBe(true)
    expect(engineSupports({ kind: 'offline', model: 'paddleocr_vl_1_6' }, 'markdown')).toBe(true)
    expect(engineSupports({ kind: 'cloud', model: 'PP-OCRv6' }, 'markdown')).toBe(false)
    expect(engineSupports({ kind: 'cloud', model: 'PP-StructureV3' }, 'markdown')).toBe(true)
    expect(engineSupports({ kind: 'llm' }, 'markdown')).toBe(true)
    expect(engineSupports({ kind: 'offline', model: 'pp_ocrv6_tiny' }, 'translate')).toBe(true)
  })
})

describe('下载进度', () => {
  it('确定进度只在下载中 / 已暂停时有；剩余时间按速度估算，速度为 0 或超过一天时不给出', () => {
    const downloading = { state: 'downloading', done: 25, total: 100, bytes_per_sec: 5 } as const
    expect(progressOf(downloading)).toBe(0.25)
    expect(progressOf({ state: 'paused', done: 100, total: 0 })).toBe(0)
    expect(progressOf({ state: 'verifying' })).toBeNull()
    expect(etaSeconds(downloading)).toBe(15)
    expect(etaSeconds({ ...downloading, bytes_per_sec: 0 })).toBeNull()
    expect(etaSeconds({ ...downloading, total: 1e12, bytes_per_sec: 1 })).toBeNull()
    expect(etaSeconds({ state: 'paused', done: 1, total: 2 })).toBeNull()
  })

  it('下载前确认：只要不是「适合本机」都先确认（含显卡待检测）；继续已暂停的下载只在检测出较慢 / 内存不足时确认', () => {
    expect(needsFitConfirm('good')).toBe(false)
    expect(['slow', 'insufficient', 'unknown'].every((l) => needsFitConfirm(l as 'slow'))).toBe(
      true,
    )
    expect(needsResumeConfirm('unknown')).toBe(false)
    expect(needsResumeConfirm('slow')).toBe(true)
    expect(needsResumeConfirm('insufficient')).toBe(true)
  })

  it('能直接用：模型已安装且推理组件都装好', () => {
    const installed = { state: 'installed' } as const
    expect(isUsable({ state: installed, pending_runtime_bytes: 0 })).toBe(true)
    expect(isUsable({ state: installed, pending_runtime_bytes: 1 })).toBe(false)
    expect(isUsable({ state: { state: 'broken' }, pending_runtime_bytes: 0 })).toBe(false)
  })
})

describe('引导页的两档组合', () => {
  const withFit = (
    s: LocalModelsSnapshot,
    patch: Partial<Record<string, LocalModelInfo['fitness']>>,
  ): LocalModelsSnapshot => ({
    ...s,
    models: s.models.map((m) => ({ ...m, fitness: patch[m.id] ?? m.fitness })),
  })

  it('高质量的两个模型都适合本机时推荐高质量，否则推荐轻量；一档的适配取较差的那个', () => {
    const all = snap()
    const good = {
      ...all,
      models: all.models.map((m) => ({
        ...m,
        fitness: { level: 'good' as const, reason_key: null },
      })),
    }
    expect(recommendedPack(good.models)).toBe('quality')
    const low = withFit(good, {
      index_translate_9b: {
        level: 'insufficient',
        reason_key: 'settings.offline.fitness.low_memory',
      },
    })
    expect(packFitness('quality', low.models)).toBe('insufficient')
    expect(recommendedPack(low.models)).toBe('light')
    const unknown = withFit(good, { paddleocr_vl_1_6: { level: 'unknown', reason_key: null } })
    expect(recommendedPack(unknown.models)).toBe('light')
  })

  it('总量：已安装的模型不计；两个模型共用的推理组件只算一次', () => {
    const s = snap()
    const runtimes = [
      { id: 'llama_cpp' as const, installed: false, size: 10 },
      { id: 'onnx_runtime' as const, installed: false, size: 20 },
    ]
    const models = s.models.map((m) => ({ ...m, state: { state: 'not_installed' as const } }))
    const vl = models.find((m) => m.id === 'paddleocr_vl_1_6')?.download_bytes ?? 0
    const nine = models.find((m) => m.id === 'index_translate_9b')?.download_bytes ?? 0
    expect(packSize('quality', models, runtimes)).toBe(vl + nine + 30)
    const installed = models.map((m) =>
      m.id === 'index_translate_9b' ? { ...m, state: { state: 'installed' as const } } : m,
    )
    expect(packSize('quality', installed, runtimes)).toBe(vl + 30)
    expect(
      packSize(
        'light',
        models,
        runtimes.map((r) => ({ ...r, installed: true })),
      ),
    ).toBe(
      (models.find((m) => m.id === 'pp_ocrv6_small')?.download_bytes ?? 0) +
        (models.find((m) => m.id === 'index_translate_2b')?.download_bytes ?? 0),
    )
  })
})
