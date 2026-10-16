/**
 * 「用哪个模型」的选择逻辑：离线模型只写本机的 `offline.*`，其他写同步的 `defaults.*` 并清空本机的离线选择；
 * 文字识别服务（PaddleOCR）只出现在 OCR 的候选里；引导「模型步骤已完成」接受离线翻译模型。
 */
import { describe, expect, it } from 'vitest'
import { Settings as Golden } from '../../../shared/ipc/fixtures'
import type { Provider, Settings } from '../../../shared/ipc/types'
import { entry, provider } from '../../__tests__/harness'
import {
  choicePatch,
  currentValue,
  offlineCandidates,
  offlineOfValue,
  offlineValue,
  translateReady,
} from '../engine'
import { fillDefaults, ocrOptions, refKey, translateOptions } from '../model'

const base = (over: Partial<Settings> = {}): Settings => ({
  ...(structuredClone(Golden) as Settings),
  ...over,
})
const paddle: Provider = {
  ...provider('paddle'),
  protocol: 'paddle_ocr',
  models: [entry('PP-OCRv6', 'unknown'), entry('PP-StructureV3', 'unknown')],
}
const openai = provider('openai', { models: [entry('gpt-5.4-mini'), entry('blind', 'no')] })

describe('写入：离线 → offline.*；其他 → defaults.* 并清空离线', () => {
  it('选离线模型只写本机的 offline.*_model', () => {
    const s = base()
    expect(choicePatch(s, 'translate', offlineValue('index_translate_2b'))).toEqual({
      offline: { translate_model: 'index_translate_2b' },
    })
    expect(choicePatch(s, 'ocr', offlineValue('pp_ocrv6_small'))).toEqual({
      offline: { ocr_model: 'pp_ocrv6_small' },
    })
  })

  it('选云端模型写 defaults；本机选过离线模型时一并清空，否则不碰 offline', () => {
    const ref = { provider_id: 'openai', model_id: 'gpt-5.4-mini' }
    const value = refKey(ref) as string
    expect(choicePatch(base(), 'translate', value)).toEqual({ defaults: { translate: ref } })
    const local = base({ offline: { ...base().offline, ocr_model: 'pp_ocrv6_tiny' } })
    expect(choicePatch(local, 'ocr', value)).toEqual({
      defaults: { ocr: ref },
      offline: { ocr_model: null },
    })
  })

  it('当前值：本机的离线模型优先于 defaults', () => {
    const ref = { provider_id: 'openai', model_id: 'gpt-5.4-mini' }
    const s = base({ defaults: { translate: ref, ocr: null } })
    expect(currentValue(s, 'translate')).toBe(refKey(ref))
    const local = {
      ...s,
      offline: { ...s.offline, translate_model: 'index_translate_9b' as const },
    }
    expect(offlineOfValue(currentValue(local, 'translate') as string)).toBe('index_translate_9b')
    expect(currentValue(s, 'ocr')).toBeNull()
  })

  it('离线候选按任务分开、按界面顺序；还没读到库存时也列出（大小未知）', () => {
    expect(offlineCandidates('ocr', null).map((c) => c.id)).toEqual([
      'pp_ocrv6_tiny',
      'pp_ocrv6_small',
      'paddleocr_vl_1_6',
    ])
    expect(offlineCandidates('translate', null).map((c) => [c.id, c.info])).toEqual([
      ['index_translate_2b', null],
      ['index_translate_9b', null],
    ])
  })
})

describe('候选：文字识别服务只用于 OCR', () => {
  it('翻译候选不含 PaddleOCR；OCR 候选含它（按协议视为支持图片，排在前面），LLM 仍按 vision 过滤', () => {
    expect(translateOptions([paddle, openai]).map((o) => o.label)).toEqual([
      'gpt-5.4-mini',
      'blind',
    ])
    const ocr = ocrOptions([openai, paddle])
    expect(ocr.map((o) => o.label)).toEqual(['PP-OCRv6', 'PP-StructureV3', 'gpt-5.4-mini'])
    expect(ocr.filter((o) => o.service).every((o) => o.vision === 'yes')).toBe(true)
  })

  it('新加的 PaddleOCR 只补默认 OCR 模型，不补翻译', () => {
    const s = base({ defaults: { translate: null, ocr: null } })
    expect(fillDefaults(s, paddle)).toEqual({
      translate: null,
      ocr: { provider_id: 'paddle', model_id: 'PP-OCRv6' },
    })
    const taken = base({ defaults: { translate: null, ocr: { provider_id: 'x', model_id: 'y' } } })
    expect(fillDefaults(taken, paddle)).toBeNull()
  })
})

describe('引导：模型步骤已完成', () => {
  it('接入服务并选了默认翻译模型，或者本机选用了离线翻译模型', () => {
    expect(translateReady(null)).toBe(false)
    expect(translateReady(base())).toBe(false)
    const cloud = base({
      providers: [openai],
      defaults: { translate: { provider_id: 'openai', model_id: 'gpt-5.4-mini' }, ocr: null },
    })
    expect(translateReady(cloud)).toBe(true)
    const local = base({ offline: { ...base().offline, translate_model: 'index_translate_2b' } })
    expect(translateReady(local)).toBe(true)
  })
})
