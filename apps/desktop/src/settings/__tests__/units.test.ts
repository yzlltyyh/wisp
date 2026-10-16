/**
 * 纯函数与 store：文案完整性（zh-CN / en-US 键集合与占位符一致、wisp-llm 下发的模板 / 区域 key 齐全）、
 * 服务与模型的辅助函数（OCR 只列 Yes / Unknown；新服务补默认模型；URL 占位）、风格变量提示、
 * 设置写入的乐观更新、回滚与合并写入。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ONBOARDING_MESSAGES } from '../../onboarding/i18n'
import { stepFromNumber, stepsFor } from '../../onboarding/OnboardingApp'
import { SETTINGS_MESSAGES } from '../i18n'
import type { HotkeyAction, OsKind } from '../../shared/ipc/types'
import { HOTKEY_DEFAULTS, shownCombo } from '../lib/hotkeyDefaults'
import { at, mergePatch } from '../lib/patch'
import { radioGroupKeys, rovingTabIndex } from '../lib/radio'
import { pendingWriteCount, resetWrites, settleWrites } from '../lib/writes'
import { modelRefState, unknownVariables } from '../lib/styles'
import { previewModelLabel } from '../pages/AppearancePage'
import { isNonChatModel } from '../../shared/models'
import {
  defaultIsValid,
  fillDefaults,
  hostOf,
  initialModels,
  ocrOptions,
  isLoopbackUrl,
  placeholderRange,
  providerNeedsKey,
  refKey,
  replaceProvider,
  translateOptions,
} from '../providers/model'
import {
  applySnapshot,
  dropQueued,
  flushQueuedPatches,
  patchSettings,
  queuePatch,
  resetSettingsStore,
  setSaveErrorHandler,
  useSettingsStore,
  loadSettings,
} from '../store'
import { entry, installHarness, ipcError, provider, snapshot } from './harness'

function flatten(obj: object, prefix = '', out = new Map<string, string>()): Map<string, string> {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k
    if (typeof v === 'string') out.set(key, v)
    else flatten(v as object, key, out)
  }
  return out
}

const placeholders = (s: string) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort()

describe('文案', () => {
  for (const [ns, messages] of [
    ['settings', SETTINGS_MESSAGES],
    ['onboarding', ONBOARDING_MESSAGES],
  ] as const) {
    it(`${ns}：两种语言的键集合与占位符一致`, () => {
      const zh = flatten(messages['zh-CN'])
      const en = flatten(messages['en-US'])
      expect([...en.keys()].sort()).toEqual([...zh.keys()].sort())
      for (const [k, v] of zh) expect(placeholders(en.get(k) ?? ''), k).toEqual(placeholders(v))
    })
  }

  it('wisp-llm 下发的 17 个模板名与 3 个区域名都在', () => {
    const zh = flatten(SETTINGS_MESSAGES['zh-CN'])
    const templates = [...zh.keys()].filter((k) => k.startsWith('providers.template.'))
    const regions = [...zh.keys()].filter((k) => k.startsWith('providers.region.'))
    expect(templates).toHaveLength(17)
    expect(regions).toEqual([
      'providers.region.cn',
      'providers.region.intl',
      'providers.region.workspace',
    ])
  })
})

describe('补丁', () => {
  it('at() 由路径构造嵌套补丁；mergePatch 合并对象、整体替换数组、null 写成 null', () => {
    expect(at('translate.anchor', 'cursor')).toEqual({ translate: { anchor: 'cursor' } })
    const merged = mergePatch(
      { a: { b: 1, c: [1, 2] }, d: 7 as number | null },
      { a: { c: [3] }, d: null },
    )
    expect(merged).toEqual({ a: { b: 1, c: [3] }, d: null })
  })
})

describe('服务与模型', () => {
  const p = provider('openai', {
    models: [
      entry('gpt-mini', 'yes'),
      entry('text-only', 'no'),
      entry('maybe', 'unknown'),
      entry('off', 'yes', { enabled: false }),
    ],
  })

  it('providerNeedsKey 与 Rust 的 provider_needs_key 同一规则：模板声明优先，没有模板时回环地址不需要 Key', () => {
    const local = { template_id: null, base_url: 'http://localhost:11434/v1' }
    expect(providerNeedsKey(local, undefined)).toBe(false)
    for (const base_url of [
      '127.0.0.1:1234',
      'http://[::1]:8080',
      'http://127.0.0.2/v1',
      'http://user:pw@LOCALHOST:8080/v1',
      'http://ollama.localhost/v1',
    ])
      expect(isLoopbackUrl(base_url), base_url).toBe(true)
    for (const base_url of [
      'https://api.openai.com/v1',
      '',
      'http://localhost.example.com/v1',
      'http://128.0.0.1',
    ])
      expect(isLoopbackUrl(base_url), base_url).toBe(false)
    expect(
      providerNeedsKey({ template_id: null, base_url: 'https://api.openai.com/v1' }, null),
    ).toBe(true)
    // 模板声明优先于地址；模板表里查不到（还没加载 / 未知模板）时按地址
    expect(
      providerNeedsKey(
        { template_id: 'ollama', base_url: 'http://10.0.0.2:11434' },
        { key_required: false },
      ),
    ).toBe(false)
    expect(
      providerNeedsKey(
        { template_id: 'openai', base_url: 'http://127.0.0.1:8080' },
        { key_required: true },
      ),
    ).toBe(true)
    expect(
      providerNeedsKey({ template_id: 'gone', base_url: 'http://127.0.0.1:8080' }, undefined),
    ).toBe(false)
  })

  it('hostOf：去掉 scheme 与末尾 #，保留端口；没写 scheme 也能解析', () => {
    expect(hostOf('https://api.openai.com/v1')).toBe('api.openai.com')
    expect(hostOf('http://localhost:11434#')).toBe('localhost:11434')
    expect(hostOf('api.deepseek.com')).toBe('api.deepseek.com')
    expect(hostOf('')).toBe('')
  })

  it('默认模型候选：翻译列全部已启用；OCR 去掉 vision = no', () => {
    expect(translateOptions([p]).map((o) => o.label)).toEqual(['gpt-mini', 'text-only', 'maybe'])
    expect(ocrOptions([p]).map((o) => o.label)).toEqual(['gpt-mini', 'maybe'])
    const textOnly = { provider_id: 'openai', model_id: 'text-only' }
    expect(defaultIsValid(textOnly, ocrOptions([p]))).toBe(false)
    expect(defaultIsValid(textOnly, translateOptions([p]))).toBe(true)
    expect(defaultIsValid(null, [])).toBe(true)
    expect(refKey(textOnly)).toBe('openai\u0000text-only')
  })

  it('新服务补上缺失的默认模型：OCR 优先确认支持图片的；已有默认值不动', () => {
    const settings = snapshot().settings
    expect(fillDefaults(settings, p)).toEqual({
      translate: { provider_id: 'openai', model_id: 'gpt-mini' },
      ocr: { provider_id: 'openai', model_id: 'gpt-mini' },
    })
    const withDefaults = {
      ...settings,
      defaults: {
        translate: { provider_id: 'x', model_id: 'y' },
        ocr: { provider_id: 'x', model_id: 'z' },
      },
    }
    expect(fillDefaults(withDefaults, p)).toBeNull()
  })

  it('非对话模型（嵌入 / 语音 / 图像生成）不当默认模型：列表第一项是 embedding 时翻译与 OCR 都选 gpt-4o', () => {
    const settings = snapshot().settings
    const relay = provider('relay', {
      models: [
        entry('text-embedding-3-small', 'no'),
        entry('nomic-embed-text', 'unknown'),
        entry('gpt-4o', 'yes'),
        entry('whisper-1', 'no'),
      ],
    })
    expect(fillDefaults(settings, relay)).toEqual({
      translate: { provider_id: 'relay', model_id: 'gpt-4o' },
      ocr: { provider_id: 'relay', model_id: 'gpt-4o' },
    })
    // 候选列表里也没有它们（默认模型下拉）
    expect(translateOptions([relay]).map((o) => o.label)).toEqual(['gpt-4o'])
    expect(ocrOptions([relay]).map((o) => o.label)).toEqual(['gpt-4o'])
    // 全是非对话模型：不硬塞一个会失败的默认值
    const onlyEmbed = provider('e', { models: [entry('text-embedding-3-small', 'no')] })
    expect(fillDefaults(settings, onlyEmbed)).toBeNull()
  })

  it('新服务的默认翻译模型优先用测试连接探测成功的模型（model_used）；OCR 只在它确认支持图片时跟随', () => {
    const settings = snapshot().settings
    const p2 = provider('p2', {
      models: [
        entry('nomic-embed-text', 'unknown'),
        entry('llama3', 'unknown'),
        entry('qwen2.5vl', 'yes'),
      ],
    })
    expect(fillDefaults(settings, p2, 'qwen2.5vl')).toEqual({
      translate: { provider_id: 'p2', model_id: 'qwen2.5vl' },
      ocr: { provider_id: 'p2', model_id: 'qwen2.5vl' },
    })
    expect(fillDefaults(settings, p2, 'llama3')).toEqual({
      translate: { provider_id: 'p2', model_id: 'llama3' },
      ocr: { provider_id: 'p2', model_id: 'qwen2.5vl' },
    })
    // 探测到的模型不在启用列表里（或是非对话模型）：退回第一个对话模型
    expect(fillDefaults(settings, p2, 'nomic-embed-text')?.translate).toEqual({
      provider_id: 'p2',
      model_id: 'llama3',
    })
  })

  it('第一次拿到模型列表时非对话模型默认不启用；之后的刷新原样采用 Rust 的合并结果', () => {
    const fetched = [entry('text-embedding-3-small', 'no'), entry('gpt-4o', 'yes')]
    expect(initialModels([], fetched).map((m) => [m.id, m.enabled])).toEqual([
      ['text-embedding-3-small', false],
      ['gpt-4o', true],
    ])
    expect(initialModels([entry('gpt-4o', 'yes')], fetched)).toBe(fetched)
  })

  it('isNonChatModel：与 Rust 同一套规则（去掉 provider 前缀、按分段匹配）', () => {
    for (const id of [
      'text-embedding-3-small',
      'nomic-embed-text:latest',
      'openai/whisper-1',
      'tts-1-hd',
      'gpt-4o-realtime-preview',
      'gpt-4o-audio-preview',
      'dall-e-3',
      'gpt-image-1',
      'omni-moderation-latest',
      'gemini-2.0-flash-live-001',
      'models/imagen-3.0-generate-002',
      'bge-reranker-v2-m3',
      'babbage-002',
    ])
      expect(isNonChatModel(id), id).toBe(true)
    for (const id of [
      'gpt-4o',
      'claude-sonnet-4-5',
      'qwen2.5vl:7b',
      'deepseek-chat',
      'llama3',
      'o4-mini',
    ])
      expect(isNonChatModel(id), id).toBe(false)
  })

  it('isNonChatModel 的正则与 crates/wisp-llm/src/vision.rs 的 NON_CHAT 逐字一致', () => {
    const proc = (
      globalThis as unknown as {
        process: {
          getBuiltinModule(name: 'node:fs'): { readFileSync(p: string, e: 'utf8'): string }
          cwd(): string
        }
      }
    ).process
    const fs = proc.getBuiltinModule('node:fs')
    const rust = fs.readFileSync(`${proc.cwd()}/../../crates/wisp-llm/src/vision.rs`, 'utf8')
    const block = /static NON_CHAT[^]*?concat!\(([^]*?)\)\)/.exec(rust)?.[1] ?? ''
    const pattern = [...block.matchAll(/r"([^"]*)"/g)].map((m) => m[1]).join('')
    expect(pattern.length).toBeGreaterThan(0)
    const ts = fs.readFileSync(`${proc.cwd()}/src/shared/models.ts`, 'utf8')
    const tsPattern = /const NON_CHAT =\s*\/(.*)\/\n/.exec(ts)?.[1]
    expect(tsPattern).toBe(pattern)
  })

  it('replaceProvider 按 id 替换或追加', () => {
    const settings = { ...snapshot().settings, providers: [provider('a'), provider('b')] }
    expect(replaceProvider(settings, provider('b', { name: 'B' })).map((x) => x.name)).toEqual([
      'a',
      'B',
    ])
    expect(replaceProvider(settings, provider('c')).map((x) => x.id)).toEqual(['a', 'b', 'c'])
  })

  it('URL 里的 {WorkspaceId} 占位能被找到（选中后直接输入替换）', () => {
    const url = 'https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1'
    expect(placeholderRange(url)).toEqual([8, 21])
    expect(placeholderRange('https://api.openai.com/v1')).toBeNull()
  })
})

describe('风格', () => {
  it('只认 {{source_lang}} / {{target_lang}}，其余变量提示（去重）', () => {
    expect(unknownVariables('A {{source_lang}} {{ product }} {{text}} {{product}}')).toEqual([
      'product',
      'text',
    ])
    expect(unknownVariables('{{source_lang}} → {{target_lang}}')).toEqual([])
  })

  it('绑定的模型被删除 / 停用时回退', () => {
    const providers = [
      provider('a', { models: [entry('m1'), entry('m2', 'yes', { enabled: false })] }),
    ]
    expect(modelRefState({ provider_id: 'a', model_id: 'm1' }, providers)).toBe('ok')
    expect(modelRefState({ provider_id: 'a', model_id: 'm2' }, providers)).toBe('disabled')
    expect(modelRefState({ provider_id: 'a', model_id: 'gone' }, providers)).toBe('missing')
    expect(modelRefState({ provider_id: 'b', model_id: 'm1' }, providers)).toBe('missing')
  })
})

describe('引导步骤', () => {
  it('macOS 有权限步骤，其他平台没有；编号 1 在其他平台落到接入模型', () => {
    expect(stepsFor('macos')).toEqual(['welcome', 'permissions', 'model', 'try'])
    expect(stepsFor('windows')).toEqual(['welcome', 'model', 'try'])
    expect(stepFromNumber(1, 'macos')).toBe('permissions')
    expect(stepFromNumber(1, 'linux')).toBe('model')
    expect(stepFromNumber(3, 'windows')).toBe('try')
    expect(stepFromNumber(9, 'macos')).toBe('try')
    expect(stepFromNumber(null, 'macos')).toBeNull()
    expect(stepFromNumber(-1, 'macos')).toBeNull()
  })
})

describe('store：写入设置', () => {
  beforeEach(() => resetSettingsStore())
  afterEach(() => {
    vi.useRealTimers()
    resetSettingsStore()
  })

  it('乐观更新，返回后以 Rust 的快照为准（夹紧后的值）', async () => {
    const h = installHarness()
    h.handle('settings_patch', ({ patch }) => {
      const snap = snapshot()
      const p = patch as { appearance: { font_size: number } }
      snap.settings.appearance.font_size = Math.min(18, p.appearance.font_size)
      return snap
    })
    await loadSettings()
    const pending = patchSettings(at('appearance.font_size', 30))
    expect(useSettingsStore.getState().snap?.settings.appearance.font_size).toBe(30)
    await pending
    expect(useSettingsStore.getState().snap?.settings.appearance.font_size).toBe(18)
    expect(useSettingsStore.getState().pending).toBe(0)
  })

  it('失败时提示并回到服务器状态', async () => {
    const h = installHarness()
    const onError = vi.fn<(error: unknown) => void>()
    setSaveErrorHandler(onError)
    await loadSettings()
    h.handle('settings_patch', () => {
      throw ipcError('invalid_argument', 'appearance.accent: invalid value')
    })
    const ok = await patchSettings(at('appearance.accent', 'nope'))
    expect(ok).toBe(false)
    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ code: 'invalid_argument' }))
    expect(useSettingsStore.getState().snap?.settings.appearance.accent).toBe('system')
  })

  it('在途期间到达的 settings:changed 不覆盖乐观值', async () => {
    installHarness()
    await loadSettings()
    let release: () => void = () => {}
    const h2 = installHarness(snapshot())
    h2.handle(
      'settings_patch',
      ({ patch }) =>
        new Promise((resolve) => {
          release = () =>
            resolve(
              snapshot({
                general: {
                  ...snapshot().settings.general,
                  ...(patch as { general: object }).general,
                },
              }),
            )
        }),
    )
    const pending = patchSettings(at('general.start_hidden', false))
    applySnapshot(snapshot())
    expect(useSettingsStore.getState().snap?.settings.general.start_hidden).toBe(false)
    release()
    await pending
    expect(useSettingsStore.getState().snap?.settings.general.start_hidden).toBe(false)
  })

  it('合并写入：同一 key 连续改动只发最后一次', async () => {
    vi.useFakeTimers()
    const h = installHarness()
    await loadSettings()
    queuePatch('font', at('appearance.font_size', 13))
    queuePatch('font', at('appearance.font_size', 15))
    queuePatch('font', at('appearance.font_size', 16))
    expect(useSettingsStore.getState().snap?.settings.appearance.font_size).toBe(16)
    expect(h.callsOf('settings_patch')).toHaveLength(0)
    await vi.advanceTimersByTimeAsync(400)
    expect(h.callsOf('settings_patch')).toEqual([{ patch: { appearance: { font_size: 16 } } }])
    queuePatch('font', at('appearance.font_size', 12))
    flushQueuedPatches()
    expect(h.callsOf('settings_patch')).toHaveLength(2)
  })
})

describe('合并写入的补丁按发出时的快照计算', () => {
  beforeEach(() => {
    resetSettingsStore()
    resetWrites()
  })

  it('build 在发出时才算整个数组；dropQueued 丢掉还没发出的补丁', async () => {
    vi.useFakeTimers()
    const h = installHarness(snapshot({ providers: [provider('a'), provider('b')] }))
    await loadSettings()
    const build = () => ({ providers: useSettingsStore.getState().snap?.settings.providers })
    queuePatch('provider:a', { providers: [provider('a', { name: 'A2' }), provider('b')] }, build)
    // 期间 b 被删掉了（本地快照已更新）：发出时按当时的数组
    const snap = useSettingsStore.getState().snap
    if (snap)
      useSettingsStore.setState({
        snap: {
          ...snap,
          settings: { ...snap.settings, providers: [provider('a', { name: 'A2' })] },
        },
      })
    await vi.advanceTimersByTimeAsync(400)
    const sent = h.callsOf('settings_patch')[0]?.patch as { providers: Array<{ id: string }> }
    expect(sent.providers.map((p) => p.id)).toEqual(['a'])
    queuePatch('provider:a', { providers: [provider('a', { name: 'A3' })] }, build)
    expect(useSettingsStore.getState().pending).toBe(1)
    dropQueued('provider:a')
    expect(useSettingsStore.getState().pending).toBe(0)
    await vi.advanceTimersByTimeAsync(400)
    expect(h.callsOf('settings_patch')).toHaveLength(1)
  })

  it('settleWrites 先提交排队的补丁，再等在途写入返回', async () => {
    const h = installHarness()
    await loadSettings()
    let release: () => void = () => {}
    h.handle(
      'settings_patch',
      () =>
        new Promise((done) => {
          release = () => done(structuredClone(h.state.snap))
        }),
    )
    queuePatch('font', at('appearance.font_size', 15))
    let settled = false
    const done = settleWrites().then(() => {
      settled = true
    })
    await vi.waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    expect(pendingWriteCount()).toBe(1)
    expect(settled).toBe(false)
    release()
    await done
    expect(settled).toBe(true)
    expect(pendingWriteCount()).toBe(0)
  })
})

describe('默认快捷键表与 Rust 一致', () => {
  it('HOTKEY_DEFAULTS 取自 Rust 生成的 HotkeyDefaults.json：三端 × 五个动作都有首选与备选', () => {
    // 与 defaults.rs 逐项一致由 Rust 侧把关（cargo test -p wisp-types --test consts）；这里核对形状与可用性
    const actions: HotkeyAction[] = [
      'translate_selection',
      'translate_input',
      'ocr',
      'ocr_translate',
      'clipboard',
    ]
    for (const os of ['macos', 'windows', 'linux'] as OsKind[]) {
      expect(Object.keys(HOTKEY_DEFAULTS[os]).toSorted()).toEqual(actions.toSorted())
      const preferred = actions.map((a) => HOTKEY_DEFAULTS[os][a].preferred)
      expect(new Set(preferred).size).toBe(actions.length)
      for (const a of actions) {
        expect(HOTKEY_DEFAULTS[os][a].preferred).toMatch(
          /^(?:(?:Control|Alt|Shift|Super)\+)+Key[A-Z]$/,
        )
        expect(HOTKEY_DEFAULTS[os][a].fallback).toMatch(
          /^(?:(?:Control|Alt|Shift|Super)\+)+Key[A-Z]$/,
        )
      }
    }
    expect(HOTKEY_DEFAULTS.macos.translate_selection).toEqual({
      preferred: 'Alt+KeyD',
      fallback: 'Control+Alt+KeyD',
    })
  })

  it('没注册上的默认绑定：显示 Rust 尝试的首选组合', () => {
    const status = {
      target: { kind: 'action', action: 'ocr' },
      binding: 'default',
      effective: null,
      display: null,
      state: 'failed',
      reason_key: 'hotkeys.reason.taken',
    } as const
    expect(shownCombo(status, 'windows')).toBe('Control+Alt+KeyS')
    expect(shownCombo({ ...status, binding: 'Alt+KeyQ' }, 'windows')).toBe('Alt+KeyQ')
    expect(shownCombo({ ...status, binding: 'none', state: 'unbound' }, 'windows')).toBeNull()
  })
})

describe('自绘单选组的键盘约定', () => {
  it('roving tabindex 与方向键 / Home / End', () => {
    expect([0, 1, 2].map((i) => rovingTabIndex(i, 1))).toEqual([-1, 0, -1])
    expect([0, 1, 2].map((i) => rovingTabIndex(i, -1))).toEqual([0, -1, -1])
    const picked: number[] = []
    const focused: number[] = []
    const onKey = radioGroupKeys(
      3,
      2,
      (i) => picked.push(i),
      (i) => focused.push(i),
    )
    const key = (k: string) => onKey({ key: k, preventDefault() {}, stopPropagation() {} } as never)
    key('ArrowRight')
    key('ArrowLeft')
    key('Home')
    key('End')
    key('a')
    expect(picked).toEqual([0, 1, 0, 2])
    expect(focused).toEqual(picked)
  })
})

describe('外观预览的模型名（与面板状态行同一种显示名）', () => {
  it('默认翻译模型的 label（空白时用 id）；没有默认模型 / 找不到时用固定示例名', () => {
    const providers = [
      provider('openai', { models: [entry('gpt-5.4-mini', 'yes', { label: 'GPT-5.4 mini' })] }),
      provider('local', { models: [entry('qwen3', 'no', { label: '  ' })] }),
    ]
    const ref = (provider_id: string, model_id: string) => ({ provider_id, model_id })
    expect(
      previewModelLabel({
        providers,
        defaults: { translate: ref('openai', 'gpt-5.4-mini'), ocr: null },
      }),
    ).toBe('GPT-5.4 mini')
    expect(
      previewModelLabel({ providers, defaults: { translate: ref('local', 'qwen3'), ocr: null } }),
    ).toBe('qwen3')
    expect(previewModelLabel({ providers, defaults: { translate: null, ocr: null } })).toBe(
      'GPT-5.4 mini',
    )
    expect(
      previewModelLabel({ providers, defaults: { translate: ref('gone', 'x'), ocr: null } }),
    ).toBe('GPT-5.4 mini')
  })
})
