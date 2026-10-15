/**
 * 设置窗口的纯函数与 store 行为：目标语言对调、直接写入时去掉排队补丁里的同一路径、轻提示停留时长来自令牌并且
 * 悬停暂停、中文列表的中西文留白、两字母头像与重名编号、草稿的二次提交 / 规范化 / 丢弃、关窗与重启前放弃草稿 Key、
 * 颜色换算、重启后回到引导的同一步、快捷键测试结果绑定组合。
 */
import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { getAppearance, setAppearance } from '../../shared/hooks/useAppearance'
import { ms, timing } from '../../shared/motion/tokens'
import {
  invalidateHotkeyTest,
  resetHotkeyTest,
  startHotkeyTest,
  useHotkeyTestState,
} from '../components/hotkeyTest'
import { hexColor } from '../lib/color'
import { formatList } from '../lib/format'
import { langPairPatch } from '../lib/langs'
import { at, subtractPatch } from '../lib/patch'
import { useDraft } from '../lib/useDraft'
import { closeWindowSafely, relaunchSafely, resetWrites } from '../lib/writes'
import { monogram, uniqueName } from '../providers/model'
import {
  holdNotice,
  loadSettings,
  notify,
  patchSettings,
  queuePatch,
  resetSettingsStore,
  useSettingsStore,
} from '../store'
import { installHarness } from './harness'

const appearance = getAppearance()

beforeEach(() => {
  resetSettingsStore()
  resetWrites()
  resetHotkeyTest()
})

afterEach(() => {
  vi.useRealTimers()
  setAppearance(appearance)
})

describe('目标语言对', () => {
  it('改成另一项的值时两者对调（一次写入），否则只改这一项', () => {
    const pair = { primary_lang: 'zh-CN', secondary_lang: 'en' } as const
    expect(langPairPatch(pair, 'primary', 'en')).toEqual({
      translate: { primary_lang: 'en', secondary_lang: 'zh-CN' },
    })
    expect(langPairPatch(pair, 'primary', 'ja')).toEqual({ translate: { primary_lang: 'ja' } })
    expect(langPairPatch(pair, 'secondary', 'zh-CN')).toEqual({
      translate: { secondary_lang: 'zh-CN', primary_lang: 'en' },
    })
    expect(langPairPatch(pair, 'secondary', 'ko')).toEqual({ translate: { secondary_lang: 'ko' } })
  })
})

describe('直接写入与排队的补丁', () => {
  it('subtractPatch 去掉被写到的叶子 / 整个对象，什么都不剩时返回 null', () => {
    expect(
      subtractPatch(
        { appearance: { accent: '#123456', font_size: 15 }, translate: { anchor: 'cursor' } },
        { appearance: { accent: 'blue' } },
      ),
    ).toEqual({ appearance: { font_size: 15 }, translate: { anchor: 'cursor' } })
    expect(subtractPatch({ appearance: { accent: '#123456' } }, { appearance: null })).toBeNull()
    expect(subtractPatch({ providers: [1] }, { providers: [] })).toBeNull()
  })

  it('拖完自定义强调色 300 ms 内点预设色块：排队的自定义色不会在之后发出、把预设盖回去', async () => {
    vi.useFakeTimers()
    const h = installHarness()
    await loadSettings()
    queuePatch('appearance.accent', at('appearance.accent', '#123456'))
    queuePatch('appearance.font_size', at('appearance.font_size', 16))
    await patchSettings(at('appearance.accent', 'green'))
    await vi.advanceTimersByTimeAsync(400)
    const patches = h.callsOf('settings_patch').map((a) => a.patch)
    expect(patches).toEqual([
      { appearance: { accent: 'green' } },
      // 另一条排队的补丁（不同路径）照常发出
      { appearance: { font_size: 16 } },
    ])
    expect(useSettingsStore.getState().snap?.settings.appearance.accent).toBe('green')
  })
})

describe('轻提示', () => {
  it('停留时长来自动效令牌（noticeHold）；错误不自动消失；悬停时暂停计时', () => {
    vi.useFakeTimers()
    notify('已复制', 'success')
    vi.advanceTimersByTime(ms(timing.noticeHold) - 1)
    expect(useSettingsStore.getState().notice?.text).toBe('已复制')
    vi.advanceTimersByTime(1)
    expect(useSettingsStore.getState().notice).toBeNull()

    notify('保存失败', 'danger')
    vi.advanceTimersByTime(ms(timing.noticeHold) * 10)
    expect(useSettingsStore.getState().notice?.text).toBe('保存失败')

    notify('已添加', 'success')
    vi.advanceTimersByTime(ms(timing.noticeHold) - 100)
    holdNotice(true)
    vi.advanceTimersByTime(ms(timing.noticeHold) * 5)
    expect(useSettingsStore.getState().notice?.text).toBe('已添加')
    holdNotice(false)
    vi.advanceTimersByTime(99)
    expect(useSettingsStore.getState().notice?.text).toBe('已添加')
    vi.advanceTimersByTime(1)
    expect(useSettingsStore.getState().notice).toBeNull()
  })
})

describe('文案排版', () => {
  it('中文列表：「和」与两侧西文之间留空格，汉字之间不加；英文照常', () => {
    setAppearance({ ...appearance, uiLanguage: 'zh-CN' })
    expect(formatList(['OpenAI', 'DeepSeek', 'Ollama'])).toBe('OpenAI、DeepSeek 和 Ollama')
    expect(formatList(['自然', '简洁'])).toBe('自然和简洁')
    expect(formatList(['{{product}}', '{{team}}'])).toBe('{{product}} 和 {{team}}')
    setAppearance({ ...appearance, uiLanguage: 'en-US' })
    expect(formatList(['OpenAI', 'DeepSeek', 'Ollama'])).toBe('OpenAI, DeepSeek, and Ollama')
  })

  it('两字母头像：OpenAI / Ollama / OpenRouter 不再都是「O」；汉字取一个；括号里的说明不算', () => {
    expect(monogram('OpenAI')).toBe('OA')
    expect(monogram('Ollama（本机）')).toBe('Ol')
    expect(monogram('OpenRouter')).toBe('OR')
    expect(monogram('DeepSeek')).toBe('DS')
    expect(monogram('Google Gemini')).toBe('GG')
    expect(monogram('Qwen (Alibaba Cloud Model Studio)')).toBe('Qw')
    expect(monogram('通义千问（阿里云百炼）')).toBe('通')
    expect(monogram('')).toBe('?')
  })

  it('新建服务与已有的重名时加编号', () => {
    const existing = [
      { id: 'a', name: 'DeepSeek' },
      { id: 'b', name: 'DeepSeek 2' },
    ]
    expect(uniqueName('DeepSeek', existing)).toBe('DeepSeek 3')
    expect(uniqueName('OpenAI', existing)).toBe('OpenAI')
  })

  it('自定义取色器只接受 #rrggbb：令牌里的 rgb() / 短写法换算过来，认不出时返回空串', () => {
    expect(hexColor('#3B82F6')).toBe('#3B82F6')
    expect(hexColor('#abc')).toBe('#aabbcc')
    expect(hexColor('rgb(59 130 246)')).toBe('#3b82f6')
    expect(hexColor(' rgba(255, 0, 10, 0.5) ')).toBe('#ff000a')
    expect(hexColor('color-mix(in srgb, red, blue)')).toBe('')
  })
})

describe('文本草稿', () => {
  it('同一个 tick 里的第二次 flush 不会再提交一遍（⌘W：失焦提交后统一提交）', () => {
    const commit = vi.fn<(v: string) => void>()
    const { result } = renderHook(() => useDraft<string>('a', commit))
    act(() => result.current.set('ab'))
    act(() => {
      result.current.flush()
      result.current.flush()
    })
    expect(commit).toHaveBeenCalledTimes(1)
    expect(commit).toHaveBeenCalledWith('ab')
  })

  it('commit 返回规范化后的值时草稿改成它；discard 丢掉还没提交的输入', () => {
    vi.useFakeTimers()
    const { result } = renderHook(() => useDraft('名称', (v: string) => v.trim() || '名称'))
    act(() => result.current.set(''))
    act(() => result.current.flush())
    expect(result.current.value).toBe('名称')
    const commit = vi.fn<(v: number | null) => void>()
    const temp = renderHook(() => useDraft<number | null>(0.3, commit, { delayMs: 250 }))
    act(() => temp.result.current.set(0.8))
    act(() => temp.result.current.discard())
    act(() => {
      vi.advanceTimersByTime(500)
    })
    expect(commit).not.toHaveBeenCalled()
    expect(temp.result.current.value).toBe(0.3)
  })
})

describe('关窗 / 重启前提交改动', () => {
  it('⌘W：先等在途写入返回，再关窗', async () => {
    const h = installHarness()
    await loadSettings()
    let release: () => void = () => {}
    h.handle(
      'settings_patch',
      () =>
        new Promise((done) => {
          release = () => done(h.state.snap)
        }),
    )
    queuePatch('font', at('appearance.font_size', 16))
    const closing = closeWindowSafely()
    await vi.waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    expect(h.close).not.toHaveBeenCalled()
    release()
    await closing
    expect(h.close).toHaveBeenCalledTimes(1)
  })

  it('「重新启动 Wisp」：先提交排队的改动，再 app_relaunch(resume)；不碰凭据存储（草稿 Key 从没写进去）', async () => {
    const h = installHarness()
    await loadSettings()
    queuePatch('font', at('appearance.font_size', 16))
    await relaunchSafely({ kind: 'settings', section: 'hotkeys' })
    const order = h.calls.map(([c]) => c)
    expect(order.indexOf('settings_patch')).toBeGreaterThanOrEqual(0)
    expect(order.indexOf('app_relaunch')).toBeGreaterThan(order.indexOf('settings_patch'))
    expect(h.callsOf('app_relaunch')[0]).toEqual({
      resume: { kind: 'settings', section: 'hotkeys' },
    })
    expect(order).not.toContain('secret_delete')
  })
})

describe('快捷键测试结果绑定测试时的组合', () => {
  it('绑定变了结果作废；「已收到」停留一会儿后自动消失', async () => {
    vi.useFakeTimers()
    const h = installHarness()
    const target = { kind: 'action', action: 'translate_selection' } as const
    const { result, rerender } = renderHook(({ combo }) => useHotkeyTestState(target, combo), {
      initialProps: { combo: 'Alt+KeyD' as string | null },
    })
    await act(async () => {
      await startHotkeyTest(target, 'Alt+KeyD')
    })
    act(() => h.emit('hotkey:test_received', { target }))
    expect(result.current.phase).toBe('received')
    // 重新录成 ⌥⇧K：旧结果不再显示
    rerender({ combo: 'Alt+Shift+KeyK' })
    expect(result.current.phase).toBe('idle')
    rerender({ combo: 'Alt+KeyD' })
    expect(result.current.phase).toBe('received')
    act(() => invalidateHotkeyTest(target, 'Alt+Shift+KeyK'))
    expect(result.current.phase).toBe('idle')

    await act(async () => {
      await startHotkeyTest(target, 'Alt+KeyD')
    })
    act(() => h.emit('hotkey:test_received', { target }))
    expect(result.current.phase).toBe('received')
    act(() => {
      vi.advanceTimersByTime(ms(timing.toastViewHold))
    })
    expect(result.current.phase).toBe('idle')
  })
})
