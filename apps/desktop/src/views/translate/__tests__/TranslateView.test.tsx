/**
 * 翻译视图：接上请求与流式显示、完成态底栏、复制、固定与展开、风格切换、语言条、取消与重新生成、
 * 润色 / 校对切分（共用 fixture）、长文本确认、未就绪、错误卡片、
 * 关闭时取消、减少动效。
 */
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { StrictMode } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import fixture from '../../../../../../crates/wisp-prompts/tests/fixtures/edit_split.json'
import type { ModelRef, PanelPayload } from '../../../shared/ipc/types'
import { activeScopes } from '../../../shared/keys/keyRouter'
import { presets } from '../../../shared/motion/presets'
import { FloatingContext } from '../../../shared/ui/floating'
import type { PanelPhase, ViewProps } from '../../../panel/types'
import { boot, WispIpcError } from '../../../shared/ipc'
import { resetCatalog, styleList } from '../catalog'
import { MoreMenu, StyleMenu } from '../menus'
import { prewarmRender, presetMotion, statusLineText, TranslateView } from '../TranslateView'
import { t } from '../../../shared/i18n/i18n'
import {
  delta,
  done,
  error,
  flush,
  idleTransport,
  installFakeIpc,
  press,
  snapshot,
  start,
  type FakeIpc,
} from './harness'

let ipc: FakeIpc

beforeEach(() => {
  ipc = installFakeIpc()
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) =>
    setTimeout(() => cb(performance.now()), 0),
  )
  vi.stubGlobal('cancelAnimationFrame', (id: number) => clearTimeout(id))
})
afterEach(() => {
  idleTransport()
  vi.unstubAllGlobals()
})

type Payload = Extract<PanelPayload, { kind: 'translate' }>

function payload(patch: Partial<Payload> = {}): Payload {
  return {
    kind: 'translate',
    source_text: 'The quick brown fox jumps over the lazy dog near the riverbank this morning.',
    request_id: 'rx',
    task: 'translate',
    style_id: 'natural',
    detected_lang: 'en',
    target_lang: 'zh-CN',
    dict_mode: false,
    long_text: null,
    not_ready: null,
    selection_method: 'ax',
    ...patch,
  }
}

/** 挂载并让视图的挂载工作（微任务里接上 / 发起请求）完成 */
async function mount(p: Payload, extra: Partial<ViewProps<'translate'>> = {}) {
  const props = {
    sessionId: 3,
    payload: p,
    phase: 'shown' as PanelPhase,
    focused: true,
    host: 'panel' as const,
    frame: 'css' as const,
    placement: 'below' as const,
    maxHeight: 480,
    requestHide: vi.fn<ViewProps<'translate'>['requestHide']>(),
    switchView: vi.fn<ViewProps<'translate'>['switchView']>(),
    activate: vi.fn<ViewProps<'translate'>['activate']>(),
    ...extra,
  }
  const r = render(<TranslateView {...props} />)
  await flush()
  return {
    ...r,
    props,
    rerender: (patch: Partial<typeof props>) => r.rerender(<TranslateView {...props} {...patch} />),
  }
}

const bodyText = () => screen.getByTestId('tv-stream-body').textContent ?? ''

async function finish(id: string, text: string) {
  ipc.push(id, start(id), delta(text), done())
  await waitFor(() => expect(bodyText()).toBe(text))
}

describe('接上请求与流式显示', () => {
  it('载荷带 request_id：接上 Rust 已发出的请求；首字前是骨架，流式中是 [停止]，完成后底栏换成操作', async () => {
    await mount(payload())
    expect(ipc.calls.find(([c]) => c === 'stream_attach')?.[1]).toEqual({ request_id: 'rx' })
    expect(screen.getByTestId('skeleton')).toBeInTheDocument()
    ipc.push('rx', start('rx'), delta('敏捷的'))
    // 骨架淡出（叠在正文下层），之后卸载
    await waitFor(() => expect(screen.queryByTestId('skeleton')).toBeNull())
    expect(screen.getByTestId('tv-stop')).toBeInTheDocument()
    expect(screen.getByTestId('tv-status')).toHaveTextContent('GPT-5.4 mini')
    ipc.push('rx', delta('棕色狐狸'), done())
    await waitFor(() => expect(bodyText()).toBe('敏捷的棕色狐狸'))
    expect(screen.queryByTestId('tv-stop')).toBeNull()
    for (const id of ['tv-copy', 'tv-pin', 'tv-source-toggle', 'tv-expand'])
      expect(screen.getByTestId(id)).toBeInTheDocument()
    expect(screen.getByTestId('tv-status')).toHaveTextContent(/GPT-5\.4 mini.*1\.2 s.*20 tokens/)
    // 输入 / 输出 token：组件库的 Tooltip（悬停 / 键盘聚焦），不是原生 title；写明哪个是输入、哪个是输出
    const status = document.querySelector('.tv-status-focus') as HTMLElement
    expect(status).toHaveAttribute(
      'aria-label',
      expect.stringContaining('输入 10 · 输出 20 tokens'),
    )
    expect(status).toHaveAttribute('aria-label', expect.stringContaining('GPT-5.4 mini'))
    expect(status).toHaveAttribute('tabindex', '0')
    expect(status.closest('[title]')).toBeNull()
    // 状态行在 <output>（role=status）里：状态变化会被播报
    expect(screen.getByTestId('tv-status').closest('output')).not.toBeNull()
  })

  it('等待首字时状态行是「等待响应…」，不沿用上一次请求的说法（风格切换后同样）', async () => {
    await mount(payload())
    ipc.push('rx', start('rx'))
    expect(screen.getByTestId('tv-status')).toHaveTextContent('等待响应')
    ipc.push('rx', delta('译'), done())
    press('Digit2', { mod: true })
    await flush()
    ipc.push('r1', start('r1'))
    expect(screen.getByTestId('tv-status')).toHaveTextContent('等待响应')
    ipc.push('r1', delta('新'))
    expect(screen.getByTestId('tv-status')).toHaveTextContent('翻译中')
  })

  it('载荷没有请求（也不是长文本 / 未就绪）：自己发起，session_id 为面板会话', async () => {
    await mount(payload({ request_id: null }))
    await flush()
    expect(ipc.starts[0]).toMatchObject({ session_id: 3, style_id: 'natural', task: 'translate' })
  })

  it('截断且没有文字 → 正文区的空状态说明原因并给出重新生成 / 切换模型 / 调高上限（状态行只留模型 · 耗时，不重复原因）；没有正文时不能固定、展开', async () => {
    await mount(payload())
    ipc.push('rx', start('rx'), done({ truncated: 'max_tokens' }))
    // 空状态已经说明原因：状态行不再用琥珀色重复，模型名完整显示
    expect(screen.getByTestId('tv-status')).not.toHaveTextContent('模型思考耗尽了输出额度')
    expect(screen.getByTestId('tv-status')).toHaveTextContent('GPT-5.4 mini')
    const empty = screen.getByTestId('tv-exhausted')
    expect(empty).toHaveTextContent('没有产出译文')
    expect(empty).toHaveTextContent('可以重新生成、切换模型，或调高输出上限。')
    // 说明里的三条出路都有按钮：「调高上限」去设置 › 模型服务
    fireEvent.click(screen.getByTestId('tv-exhausted-raise'))
    await flush()
    expect(ipc.calls.findLast(([c]) => c === 'window_open')?.[1]).toEqual({
      target: { kind: 'settings', section: 'providers' },
    })
    // 不重复「输出达到上限」的提示行
    expect(screen.queryByTestId('tv-notice')).toBeNull()
    for (const id of ['tv-pin', 'tv-expand', 'tv-copy', 'tv-regenerate'])
      expect(screen.queryByTestId(id)).toBeNull()
    press('KeyP', { mod: true })
    await flush()
    expect(ipc.names()).not.toContain('panel_pin')
    fireEvent.click(screen.getByTestId('tv-exhausted-regenerate'))
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({ bypass_cache: true })
    // 重新请求等首字时显示骨架，不留一个空的正文层
    expect(screen.getByTestId('skeleton')).toBeInTheDocument()
    expect(screen.queryByTestId('tv-stream')).toBeNull()
  })

  it('有文字的截断：提示单独一行（完整显示，可调高上限）；命中缓存：状态行「来自缓存」', async () => {
    const { unmount } = await mount(payload())
    ipc.push('rx', start('rx'), delta('半'), done({ truncated: 'max_tokens' }))
    expect(screen.getByTestId('tv-notice')).toHaveTextContent('输出达到上限，已截断')
    expect(screen.getByTestId('tv-status')).not.toHaveTextContent('截断')
    fireEvent.click(screen.getByTestId('tv-notice-action'))
    expect(ipc.calls.find(([c]) => c === 'window_open')?.[1]).toEqual({
      target: { kind: 'settings', section: 'providers' },
    })
    unmount()
    await mount(payload({ request_id: 'ry' }))
    ipc.push('ry', start('ry'), delta('缓存'), done({ cached: true, elapsed_ms: 0 }))
    expect(screen.getByTestId('tv-status')).toHaveTextContent('来自缓存')
    // 命中缓存的「0 ms」不是真实耗时：不显示
    expect(screen.getByTestId('tv-status')).not.toHaveTextContent(/\b0 ms\b/)
  })

  it('命中缓存的结果固定到 Pinned：status_line 不带耗时、注明「来自缓存」', () => {
    const snap = {
      statusLine: null,
      status: 'done',
      cached: true,
      notices: [],
      model: { label: 'Fake Model' },
      elapsedMs: 0,
      usage: null,
    } as unknown as Parameters<typeof statusLineText>[0]
    expect(statusLineText(snap, t)).toBe('Fake Model · 来自缓存')
    expect(statusLineText({ ...snap, cached: false, elapsedMs: 1200 }, t)).toBe(
      'Fake Model · 1.2 s',
    )
  })
})

describe('复制', () => {
  it('⌘C 没有选区时复制全部译文；有选区时只复制选区', async () => {
    await mount(payload())
    await finish('rx', '敏捷的棕色狐狸')
    press('KeyC', { mod: true })
    await flush()
    expect(ipc.calls.findLast(([c]) => c === 'clipboard_copy_text')?.[1]).toEqual({
      text: '敏捷的棕色狐狸',
    })
    const node = screen.getByTestId('tv-stream-body').firstChild as Node
    const range = document.createRange()
    range.setStart(node.firstChild ?? node, 0)
    range.setEnd(node.firstChild ?? node, 3)
    window.getSelection()?.removeAllRanges()
    window.getSelection()?.addRange(range)
    press('KeyC', { mod: true })
    await flush()
    expect(ipc.calls.findLast(([c]) => c === 'clipboard_copy_text')?.[1]).toEqual({
      text: '敏捷的',
    })
    await waitFor(() => expect(screen.getByTestId('tv-flash')).toHaveTextContent('已复制选中内容'))
    window.getSelection()?.removeAllRanges()
  })

  it('复制失败（例如超过 1 MB 被拒绝）：轻提示「复制失败」，临时替换状态行', async () => {
    await mount(payload())
    await finish('rx', '译文')
    ipc.handlers.clipboard_copy_text = () => {
      throw new WispIpcError('clipboard_copy_text', {
        code: 'invalid_argument',
        message: 'too large',
        llm: null,
      })
    }
    fireEvent.click(screen.getByTestId('tv-more'))
    fireEvent.click(await screen.findByText('复制原文'))
    await waitFor(() => expect(screen.getByTestId('tv-flash')).toHaveTextContent('复制失败'))
    expect(screen.getByTestId('tv-flash')).toHaveAttribute('data-tone', 'warning')
    expect(document.querySelector('.tv-status-line')).toHaveAttribute('aria-hidden', 'true')
  })
})

describe('固定与展开', () => {
  it('流式中 ⌘P：请求随 PinState 转移，之后面板关闭不再取消它', async () => {
    const { rerender } = await mount(payload())
    ipc.push('rx', start('rx'), delta('进行中'))
    press('KeyP', { mod: true })
    await flush()
    const [, args] = ipc.calls.find(([c]) => c === 'panel_pin') ?? []
    expect(args).toMatchObject({
      session_id: 3,
      expand: false,
      state: {
        kind: 'translate',
        request_id: 'rx',
        style_id: 'natural',
        target_lang: 'zh-CN',
        detected_lang: 'en',
        show_source: false,
        task: 'translate',
      },
    })
    rerender({ phase: 'exiting' })
    expect(ipc.names()).not.toContain('cancel')
  })

  it('完成后 ⤢：展开、默认显示原文对照，不转移请求；结构化状态行与实际模型随 PinState 交接', async () => {
    await mount(payload())
    await finish('rx', '译文')
    fireEvent.click(screen.getByTestId('tv-expand'))
    await flush()
    const [, args] = ipc.calls.find(([c]) => c === 'panel_pin') ?? []
    expect(args).toMatchObject({
      expand: true,
      state: { request_id: null, show_source: true, result_text: '译文' },
    })
    expect(args?.state).toMatchObject({
      status_line: expect.stringMatching(/GPT-5\.4 mini · 1\.2 s · 20 tokens/),
      dict_mode: false,
      status: { model_label: 'GPT-5.4 mini', elapsed_ms: 1234, cached: false, cancelled: false },
    })
    const pinned = args?.state as { model_ref: unknown } | undefined
    expect(pinned?.model_ref).toMatchObject({ model_id: expect.any(String) })
    // 「这是展开」由 Rust 在 pinned_init 里给出 expand_width，面板不再经同源存储交接
    expect(localStorage.length).toBe(0)
  })
})

describe('风格', () => {
  it('⌘2 切到 technical：取消进行中的请求、用新风格重新请求；⌘↓ / ⌘↑ 循环', async () => {
    await mount(payload())
    ipc.push('rx', start('rx'), delta('旧'))
    press('Digit2', { mod: true })
    await flush()
    expect(ipc.calls.find(([c]) => c === 'cancel')?.[1]).toEqual({ request_id: 'rx' })
    expect(ipc.starts.at(-1)).toMatchObject({
      style_id: 'technical',
      task: 'translate',
      bypass_cache: false,
    })
    press('ArrowDown', { mod: true })
    await flush()
    expect(ipc.starts.at(-1)?.style_id).toBe('academic')
    press('ArrowUp', { mod: true })
    await flush()
    expect(ipc.starts.at(-1)?.style_id).toBe('technical')
    expect(screen.getByTestId('tv-style-chip')).toHaveTextContent('技术')
  })

  it('风格菜单展开时裸数字 1–9 直选', async () => {
    await mount(payload())
    await finish('rx', '译文')
    fireEvent.click(screen.getByTestId('tv-style-chip'))
    await screen.findByTestId('tv-style-menu')
    press('Digit5')
    await flush()
    expect(ipc.starts.at(-1)?.style_id).toBe('learn')
  })

  it('第一次用键盘打开风格菜单：焦点进入菜单，↓ 移动高亮、Enter 选中后焦点回到 chip', async () => {
    await mount(payload())
    await finish('rx', '译文')
    const chip = screen.getByTestId('tv-style-chip')
    chip.focus()
    fireEvent.keyDown(chip, { key: 'ArrowDown' })
    const menu = await screen.findByTestId('tv-style-menu')
    await waitFor(() => expect(menu.contains(document.activeElement)).toBe(true))
    fireEvent.keyDown(menu, { key: 'ArrowDown' })
    fireEvent.keyDown(menu, { key: 'Enter' })
    await flush()
    expect(ipc.starts.at(-1)?.style_id).toBe('technical')
    expect(document.activeElement).toBe(chip)
  })

  it('裸数字直选后焦点回到 chip（不随菜单一起消失）', async () => {
    await mount(payload())
    await finish('rx', '译文')
    fireEvent.click(screen.getByTestId('tv-style-chip'))
    const menu = await screen.findByTestId('tv-style-menu')
    await waitFor(() => expect(menu.contains(document.activeElement)).toBe(true))
    press('Digit3')
    await flush()
    expect(ipc.starts.at(-1)?.style_id).toBe('academic')
    await waitFor(() => expect(document.activeElement).toBe(screen.getByTestId('tv-style-chip')))
  })

  it('同一风格再选一次不重新请求', async () => {
    await mount(payload())
    await finish('rx', '译文')
    press('Digit1', { mod: true })
    await flush()
    expect(ipc.starts).toHaveLength(0)
  })
})

describe('语言条', () => {
  it('点源语言交换方向：日文原文「JA → 中文」交换为「JA → 英语」并重新请求（只对本会话生效）', async () => {
    await mount(payload({ detected_lang: 'ja', source_text: 'これはペンです' }))
    await finish('rx', '译文')
    const source = screen.getByTestId('tv-langbar-source')
    expect(source).toHaveAttribute('aria-label', '日语 · 交换翻译方向')
    // 说明用组件库的 Tooltip，不用原生 title（不会被读两遍）
    expect(source).not.toHaveAttribute('title')
    fireEvent.click(source)
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({ target_lang: 'en' })
    expect(ipc.names()).not.toContain('settings_patch')
  })

  it('交换没有意义时禁用并说明原因：英文原文交换后目标也是英语（输出会与原文一样）', async () => {
    await mount(payload())
    await finish('rx', '译文')
    const source = screen.getByTestId('tv-langbar-source')
    expect(source).toHaveAttribute('aria-disabled', 'true')
    expect(source).toHaveAttribute('aria-label', '英语')
    fireEvent.click(source)
    await flush()
    expect(ipc.starts).toHaveLength(0)
    // 悬停（或键盘聚焦）时 Tooltip 说明原因
    fireEvent.pointerEnter(source.parentElement as HTMLElement, { pointerType: 'mouse' })
    await waitFor(() =>
      expect(screen.getByRole('tooltip')).toHaveTextContent('原文已是英语，交换方向不会改变结果'),
    )
  })

  it('检测不确定（中英混排、低置信）时可以交换：把原文当作另一种语言，源语言显示为交换前的目标', async () => {
    await mount(payload({ detected_lang: null, source_text: '混排 alpha beta gamma delta' }))
    ipc.push('rx', start('rx'), {
      type: 'meta',
      detected_source_lang: 'zh-CN',
      target_lang: 'en',
      dict_mode: false,
      low_confidence: true,
    })
    ipc.push('rx', delta('Mixed'), done())
    expect(screen.getByTestId('tv-langbar')).toHaveTextContent(/中文.*英语/)
    fireEvent.click(screen.getByTestId('tv-langbar-source'))
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({ target_lang: 'zh-CN' })
    expect(screen.getByTestId('tv-langbar-source')).toHaveTextContent('英语')
    expect(screen.getByTestId('tv-langbar-target')).toHaveTextContent('中文')
  })

  it('检测中显示「检测中…」，Meta 到达后显示检测结果与低置信提示', async () => {
    await mount(payload({ detected_lang: null }))
    expect(screen.getByTestId('tv-langbar')).toHaveTextContent('检测中')
    ipc.push('rx', start('rx'), {
      type: 'meta',
      detected_source_lang: 'ja',
      target_lang: 'zh-CN',
      dict_mode: false,
      low_confidence: true,
    })
    expect(screen.getByTestId('tv-langbar')).toHaveTextContent('日语')
    // 低置信提示点
    expect(screen.getByTestId('tv-langbar').querySelector('.ui-langbar-dot')).not.toBeNull()
  })

  it('自动方向下重选当前显示的目标：不重新请求；目标语言按钮的无障碍名称说明它是目标语言菜单', async () => {
    await mount(payload())
    await finish('rx', '译文')
    const target = screen.getByTestId('tv-langbar-target')
    expect(target).toHaveAttribute('aria-label', '中文 · 目标语言')
    fireEvent.click(target)
    fireEvent.click(await screen.findByTestId('tv-langbar-menu-item-zh-CN'))
    await flush()
    expect(ipc.starts).toHaveLength(0)
  })
})

describe('取消与重新生成', () => {
  it('⌘. 停止：保留已收到的部分、显示已取消；⌘R 用同样的文本和风格绕过缓存重新请求', async () => {
    await mount(payload())
    ipc.push('rx', start('rx'), delta('一半'))
    press('Period', { mod: true })
    expect(ipc.calls.at(-1)).toEqual(['cancel', { request_id: 'rx' }])
    await waitFor(() => expect(bodyText()).toBe('一半'))
    expect(screen.getByTestId('tv-status')).toHaveTextContent('已取消')
    // 被停止的半截译文不能「提升译文」（提升译文只针对完整的已有译文）
    fireEvent.click(screen.getByTestId('tv-more'))
    expect(await screen.findByTestId('tv-more-menu-item-refine')).toHaveAttribute(
      'aria-disabled',
      'true',
    )
    press('Escape')
    press('KeyR', { mod: true })
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({ bypass_cache: true, style_id: 'natural' })
  })

  it('面板开始关闭（exiting）时取消进行中的请求；✕ 同样先取消再关闭', async () => {
    const { rerender, props } = await mount(payload())
    ipc.push('rx', start('rx'), delta('x'))
    rerender({ phase: 'exiting' })
    expect(ipc.calls.filter(([c]) => c === 'cancel')).toHaveLength(1)
    fireEvent.click(screen.getByTestId('tv-close'))
    expect(props.requestHide).toHaveBeenCalledWith('close_button')
  })
})

describe('润色 / 校对（共用 edit_split.json）', () => {
  it.each(fixture.cases)('$name', async (c) => {
    await mount(payload({ task: 'polish' }))
    ipc.push(
      'rx',
      start('rx'),
      ...Array.from(c.input.match(/[\s\S]{1,5}/g) ?? []).map(delta),
      done(),
    )
    await waitFor(() => expect(bodyText()).toBe(c.body))
    const changes = screen.getByTestId('tv-stream-changes')
    expect(changes.hidden).toBe(c.changes === null)
    expect(changes.textContent?.replace(/\s+/g, ' ').trim()).toBe(
      (c.changes ?? '').replace(/\s+/g, ' ').trim(),
    )
    // 复制只复制正文（没有正文时无可复制）
    press('KeyC', { mod: true })
    await flush()
    const copied = ipc.calls.findLast(([cmd]) => cmd === 'clipboard_copy_text')?.[1]
    expect(copied).toEqual(c.body ? { text: c.body } : undefined)
  })

  it('⋯ 菜单的润色：同语言动作，头部换成任务标题与「返回翻译」', async () => {
    await mount(payload())
    await finish('rx', '译文')
    fireEvent.click(screen.getByTestId('tv-more'))
    fireEvent.click(await screen.findByText('润色'))
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({ task: 'polish' })
    expect(screen.getByTestId('tv-back')).toBeInTheDocument()
    fireEvent.click(screen.getByTestId('tv-back'))
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({ task: 'translate' })
  })
})

describe('长文本 / 未就绪 / 错误', () => {
  it('长文本：先确认；⌘↩ 继续翻译，取消关闭面板', async () => {
    const { props } = await mount(
      payload({
        request_id: null,
        long_text: { chars: 12000, est_tokens: 3000, est_seconds: null },
      }),
    )
    expect(screen.getByTestId('tv-long-text')).toHaveTextContent('12,000')
    expect(screen.getByTestId('tv-long-text-warning')).toBeInTheDocument()
    expect(ipc.starts).toHaveLength(0)
    press('Enter', { mod: true })
    await flush()
    expect(ipc.starts).toHaveLength(1)
    expect(screen.queryByTestId('tv-long-text')).toBeNull()
    expect(props.requestHide).not.toHaveBeenCalled()
  })

  it('长文本确认期间：风格（菜单 / ⌘数字）、任务、模型、方向只改待发请求的参数，确认卡片保留；继续后按新参数发起', async () => {
    await mount(
      payload({
        request_id: null,
        long_text: { chars: 5200, est_tokens: 1300, est_seconds: null },
      }),
    )
    // 风格菜单
    fireEvent.click(screen.getByTestId('tv-style-chip'))
    fireEvent.click(await screen.findByText('技术'))
    await flush()
    // ⌘数字
    press('Digit3', { mod: true })
    await flush()
    // ⋯ → 切换模型
    fireEvent.click(screen.getByTestId('tv-more'))
    fireEvent.click(await screen.findByText('切换模型'))
    fireEvent.click(await screen.findByText('deepseek-chat'))
    await flush()
    // ⋯ → 润色
    fireEvent.click(screen.getByTestId('tv-more'))
    fireEvent.click(await screen.findByText('润色'))
    await flush()
    expect(ipc.starts).toHaveLength(0)
    expect(screen.getByTestId('tv-long-text')).toBeInTheDocument()
    expect(screen.queryByTestId('tv-stream')).toBeNull()
    fireEvent.click(screen.getByTestId('tv-long-text-continue'))
    await flush()
    expect(ipc.starts).toHaveLength(1)
    expect(ipc.starts[0]).toMatchObject({
      task: 'polish',
      style_id: 'academic',
      model_ref: { provider_id: 'deepseek', model_id: 'deepseek-chat' },
    })
    expect(screen.queryByTestId('tv-long-text')).toBeNull()
    // 请求开始后 [停止] 可见
    ipc.push('r1', start('r1'), delta('x'))
    expect(screen.getByTestId('tv-stop')).toBeInTheDocument()
  })

  it('未就绪：NotReadyCard，「去设置」打开模型与服务；风格快捷键不发请求', async () => {
    await mount(payload({ request_id: null, not_ready: { reason: 'no_provider' } }))
    expect(screen.getByTestId('tv-not-ready')).toHaveTextContent('还没有接入模型')
    press('Digit2', { mod: true })
    await flush()
    expect(ipc.starts).toHaveLength(0)
    fireEvent.click(screen.getByText('去设置'))
    expect(ipc.calls.find(([c]) => c === 'window_open')?.[1]).toEqual({
      target: { kind: 'settings', section: 'providers' },
    })
  })

  it('Start → Error：错误卡片带服务名；重试发起新请求；流中途出错保留已收到的部分', async () => {
    await mount(payload())
    ipc.push('rx', start('rx'), error('upstream', { status: 503, retryable: true }))
    expect(screen.getByTestId('tv-error')).toHaveTextContent('503')
    expect(screen.queryByTestId('tv-footer')).toBeNull()
    fireEvent.click(screen.getByTestId('tv-error-retry'))
    await flush()
    expect(ipc.starts).toHaveLength(1)
    ipc.push('r1', start('r1'), delta('已收到'), error('network', { retryable: true }))
    await waitFor(() => expect(bodyText()).toBe('已收到'))
    expect(screen.getByTestId('tv-error')).toBeInTheDocument()
    // 有部分正文的错误态：底栏保留复制（不能固定 / 展开一个出错的结果）
    expect(screen.getByTestId('tv-copy')).toBeInTheDocument()
    expect(screen.queryByTestId('tv-pin')).toBeNull()
  })

  it('离线翻译模型不支持润色：有默认的云端翻译模型时显式「用 <模型> 处理这一次」，这一次带上它的模型引用', async () => {
    await mount(
      payload({
        request_id: null,
        task: 'polish',
        not_ready: { reason: 'offline_task_unsupported', task: 'polish' },
      }),
    )
    const use = await screen.findByTestId('tv-not-ready-use-cloud')
    expect(use).toHaveTextContent('用 GPT-5.4 mini 处理这一次')
    expect(ipc.starts).toHaveLength(0)
    fireEvent.click(use)
    await flush()
    expect(ipc.starts).toHaveLength(1)
    expect(ipc.starts[0]).toMatchObject({
      task: 'polish',
      model_ref: { provider_id: 'openai-main', model_id: 'gpt-5.4-mini' },
    })
    expect(screen.queryByTestId('tv-not-ready')).toBeNull()
  })

  it('离线翻译模型不支持润色、也没有默认的云端翻译模型：只有「去设置」（离线模型分组）', async () => {
    const s = snapshot()
    resetCatalog({
      ...s,
      settings: { ...s.settings, defaults: { ...s.settings.defaults, translate: null } },
    })
    await mount(
      payload({
        request_id: null,
        task: 'polish',
        not_ready: { reason: 'offline_task_unsupported', task: 'polish' },
      }),
    )
    fireEvent.click(await screen.findByTestId('tv-not-ready-settings'))
    expect(screen.queryByTestId('tv-not-ready-use-cloud')).toBeNull()
    expect(ipc.calls.find(([c]) => c === 'window_open')?.[1]).toEqual({
      target: { kind: 'settings', section: 'offline' },
    })
  })

  it('离线模型冷启动：骨架下方有「第一次加载需要十几秒」（由 CSS 延迟淡入），第一段文字到达后消失', async () => {
    await mount(payload())
    ipc.push('rx', start('rx'), { type: 'phase', phase: 'loading_model' })
    const note = await screen.findByTestId('tv-loading-note')
    expect(note).toHaveTextContent('第一次加载需要十几秒，之后会快很多')
    expect(note).toHaveClass('ui-late')
    ipc.push('rx', delta('你好'))
    await waitFor(() => expect(screen.queryByTestId('tv-loading-note')).toBeNull())
  })

  it('缺 Microsoft Visual C++ 运行库：错误卡片的第一个按钮打开官方下载页', async () => {
    await mount(payload())
    ipc.push(
      'rx',
      start('rx'),
      error('local_runtime', { hint: 'errors.hint.local.vc_runtime_missing' }),
    )
    const link = await screen.findByTestId('tv-error-link')
    expect(link).toHaveTextContent('下载运行库')
    fireEvent.click(link)
    expect(ipc.calls.find(([c]) => c === 'open_url')?.[1]).toEqual({
      url: 'https://learn.microsoft.com/cpp/windows/latest-supported-vc-redist',
    })
  })

  it('系统凭据存储不可用：卡片提供「去设置」（组件库）', async () => {
    await mount(payload({ request_id: null, not_ready: { reason: 'keychain_unavailable' } }))
    fireEvent.click(screen.getByTestId('tv-not-ready-settings'))
    expect(ipc.calls.find(([c]) => c === 'window_open')?.[1]).toEqual({
      target: { kind: 'settings', section: 'providers' },
    })
  })
})

describe('原文对照', () => {
  it('⇄ 显示 / 隐藏原文', async () => {
    await mount(payload())
    await finish('rx', '译文')
    expect(screen.queryByTestId('tv-source')).toBeNull()
    fireEvent.click(screen.getByTestId('tv-source-toggle'))
    expect(screen.getByTestId('tv-source')).toHaveTextContent('quick brown fox')
    fireEvent.click(screen.getByTestId('tv-source-toggle'))
    await waitFor(() => expect(screen.queryByTestId('tv-source')).toBeNull())
  })
})

describe('减少动效', () => {
  it('风格切换：正常时透明度 + 2 px 模糊，减少动效时只降透明度、≤ 100 ms', async () => {
    expect({
      out: presetMotion(presets.styleSwitchOut, false, 'out'),
      in: presetMotion(presets.styleSwitchIn, false, 'in'),
      outReduced: presetMotion(presets.styleSwitchOut, true, 'out'),
      inReduced: presetMotion(presets.styleSwitchIn, true, 'in'),
    }).toMatchSnapshot()
    // 预设没有给曲线：变暗用 ease.exit、恢复用 ease.standard（动效令牌）
    expect(presetMotion(presets.styleSwitchOut, false, 'out').transition).toMatchObject({
      ease: [0.4, 0, 1, 1],
    })
    expect(presetMotion(presets.styleSwitchIn, false, 'in').transition).toMatchObject({
      ease: [0.2, 0, 0, 1],
    })
    expect(presetMotion(presets.styleSwitchOut, true).keyframes).not.toHaveProperty('filter')
  })
})

describe('键盘作用域', () => {
  it('Esc 不由视图处理（外壳负责关闭）', async () => {
    const { props } = await mount(payload())
    act(() => {
      document.body.dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Escape', code: 'Escape', bubbles: true }),
      )
    })
    expect(props.requestHide).not.toHaveBeenCalled()
  })
})

/** 发一次按键并返回事件（看是否被 preventDefault） */
function keydown(code: string, mods: { shift?: boolean } = {}): KeyboardEvent {
  const mac = boot.os === 'macos'
  const event = new KeyboardEvent('keydown', {
    code,
    key: code.startsWith('Key') ? code.slice(3).toLowerCase() : code,
    metaKey: mac,
    ctrlKey: !mac,
    shiftKey: mods.shift === true,
    bubbles: true,
    cancelable: true,
  })
  act(() => {
    ;(document.activeElement ?? document.body).dispatchEvent(event)
  })
  return event
}

const ACCELERATORS: ReadonlyArray<[string, boolean]> = [
  ['KeyR', false],
  ['KeyR', true],
  ['KeyP', false],
]

describe('浏览器加速键（Windows WebView2 的重载 / 打印预览）', () => {
  it('长文本确认中 ⌘R / ⌘⇧R / ⌘P 都不可用：照样吞掉，不执行任何动作', async () => {
    await mount(
      payload({
        request_id: null,
        long_text: { chars: 5200, est_tokens: 1400, est_seconds: null },
      }),
    )
    for (const [code, shift] of ACCELERATORS)
      expect(keydown(code, { shift }).defaultPrevented).toBe(true)
    expect(ipc.starts).toHaveLength(0)
    expect(ipc.names()).not.toContain('panel_pin')
  })

  it('ack 之前（没有正文、也没有可转移的请求）⌘P：吞掉', async () => {
    // translate_start 一直不返回
    ipc.handlers.translate_start = () => new Promise(() => undefined)
    await mount(payload({ request_id: null }))
    expect(keydown('KeyP').defaultPrevented).toBe(true)
    expect(ipc.names()).not.toContain('panel_pin')
  })

  it('流式中 ⌘⇧R（不能提升半截译文）、出错后 ⌘P：吞掉', async () => {
    await mount(payload())
    ipc.push('rx', start('rx'), delta('半'))
    expect(keydown('KeyR', { shift: true }).defaultPrevented).toBe(true)
    ipc.push('rx', error('network', { retryable: true }))
    expect(keydown('KeyP').defaultPrevented).toBe(true)
    expect(ipc.names()).not.toContain('panel_pin')
    expect(ipc.starts.filter((r) => r.task === 'refine')).toHaveLength(0)
  })

  it('菜单打开时（modal 作用域挡住视图层）同样吞掉，不执行视图的动作', async () => {
    await mount(payload())
    await finish('rx', '译文')
    fireEvent.click(screen.getByTestId('tv-more'))
    await screen.findByTestId('tv-more-menu')
    for (const [code, shift] of ACCELERATORS)
      expect(keydown(code, { shift }).defaultPrevented).toBe(true)
    expect(ipc.starts).toHaveLength(0)
    expect(ipc.names()).not.toContain('panel_pin')
  })
})

describe('风格菜单的数字直选与焦点', () => {
  it('StrictMode 下视图挂载后第一次打开菜单：裸数字直接选中（与作用域的注册顺序无关）', async () => {
    const props = {
      sessionId: 3,
      payload: payload(),
      phase: 'shown' as PanelPhase,
      focused: true,
      host: 'panel' as const,
      frame: 'css' as const,
      placement: 'below' as const,
      maxHeight: 480,
      requestHide: vi.fn<ViewProps<'translate'>['requestHide']>(),
      switchView: vi.fn<ViewProps<'translate'>['switchView']>(),
      activate: vi.fn<ViewProps<'translate'>['activate']>(),
    }
    render(
      <StrictMode>
        <TranslateView {...props} />
      </StrictMode>,
    )
    await flush()
    await finish('rx', '译文')
    fireEvent.click(screen.getByTestId('tv-style-chip'))
    await screen.findByTestId('tv-style-menu')
    press('Digit3')
    await flush()
    expect(ipc.starts.at(-1)?.style_id).toBe('academic')
    expect(screen.getByTestId('tv-style-chip')).toHaveTextContent('学术')
    await waitFor(() => expect(screen.queryByTestId('tv-style-menu')).toBeNull())
  })

  it('数字直选：焦点立即回到 chip，菜单播放完退出动画、卸载之后仍在 chip', async () => {
    await mount(payload())
    await finish('rx', '译文')
    const chip = screen.getByTestId('tv-style-chip')
    fireEvent.click(chip)
    const menu = await screen.findByTestId('tv-style-menu')
    await waitFor(() => expect(menu.contains(document.activeElement)).toBe(true))
    press('Digit3', { mod: true })
    expect(document.activeElement).toBe(chip)
    await waitFor(() => expect(screen.queryByTestId('tv-style-menu')).toBeNull())
    expect(document.activeElement).toBe(chip)
  })

  it('风格 chip 的无障碍名称经 i18n 模板：「翻译风格：自然」', async () => {
    await mount(payload())
    expect(screen.getByTestId('tv-style-chip')).toHaveAttribute('aria-label', '翻译风格：自然')
  })
})

describe('菜单按下就打开、随卡片长高像帘子一样露出（floating 的 reserve + VisibleEdge 裁切）', () => {
  function edgeAt(initial: number) {
    let visible = initial
    const listeners = new Set<() => void>()
    return {
      edge: {
        get: () => visible,
        subscribe: (l: () => void) => {
          listeners.add(l)
          return () => listeners.delete(l)
        },
      },
      grow(to: number) {
        act(() => {
          visible = to
          for (const l of listeners) l()
        })
      },
    }
  }

  it('指针按下的那一刻就打开并预留卡片高度；卡片可见下沿之下的部分按圆角裁切，长够之后去掉裁切；关闭淡出之后才释放预留', async () => {
    const reserve = vi.fn<(id: symbol, bottom: number | null) => void>()
    const card = edgeAt(100)
    // 卡片可见的下沿由浮层边界提供（PanelCard：Css 帧读卡片、Native 帧按 Rust 同参弹簧推演）
    const boundary = {
      container: null,
      rect: () => new DOMRect(0, 0, 400, 480),
      reserve,
      edge: card.edge,
    }
    // jsdom 没有布局：菜单高 235（上下内边距 8 + 标题 22 + 6 个风格 × 28 + 分隔线 9 + 「编辑当前风格」28），顶边 0
    const isMenu = (el: HTMLElement) => el.getAttribute('role') === 'menu'
    const height = vi
      .spyOn(HTMLElement.prototype, 'offsetHeight', 'get')
      .mockImplementation(function (this: HTMLElement) {
        return isMenu(this) ? 235 : 0
      })
    const scroll = vi
      .spyOn(HTMLElement.prototype, 'scrollHeight', 'get')
      .mockImplementation(function (this: HTMLElement) {
        return isMenu(this) ? 235 : 0
      })
    try {
      render(
        <FloatingContext value={boundary}>
          <StyleMenu
            styles={styleList(null)}
            current="natural"
            onPick={vi.fn<(id: string) => void>()}
            onEdit={vi.fn<() => void>()}
          />
        </FloatingContext>,
      )
      const chip = screen.getByTestId('tv-style-chip')
      fireEvent.pointerDown(chip, { button: 0 })
      // 按下就打开（不等 click）：菜单顶边 = 触发器下沿 0 + 4，下沿 239，预留 + 8
      const menu = screen.getByTestId('tv-style-menu')
      expect(reserve).toHaveBeenCalledWith(expect.any(Symbol), 239 + 8)
      await act(async () => {})
      // 可见下沿 100：菜单从顶边起只露出 100 − 8 − 4 = 88 px（裁切写成 calc，逐帧不读布局）
      expect(menu.style.clipPath).toMatch(
        /^inset\(0 0 max\(0px, calc\(100% - 88px\)\) 0 round 0(px)?\)$/,
      )
      card.grow(200)
      expect(menu.style.clipPath).toMatch(
        /^inset\(0 0 max\(0px, calc\(100% - 188px\)\) 0 round 0(px)?\)$/,
      )
      card.grow(260)
      expect(menu.style.clipPath).toBe('')
      // 随后的 click（指针抬起）不会把刚打开的菜单关掉
      fireEvent.click(chip, { detail: 1 })
      expect(screen.getByTestId('tv-style-menu')).toBeInTheDocument()
      expect(chip).toHaveAttribute('aria-expanded', 'true')
      // 再按一次关闭：预留等菜单淡出（menuOut 100 ms）之后才释放
      fireEvent.pointerDown(chip, { button: 0 })
      expect(chip).toHaveAttribute('aria-expanded', 'false')
      expect(reserve).not.toHaveBeenLastCalledWith(expect.any(Symbol), null)
      await waitFor(() => expect(reserve).toHaveBeenLastCalledWith(expect.any(Symbol), null))
    } finally {
      height.mockRestore()
      scroll.mockRestore()
    }
  })

  it('键盘：Enter（click，detail 0）与 Space 按下都立即打开', () => {
    const boundary = {
      container: null,
      rect: () => new DOMRect(0, 0, 400, 480),
      reserve: vi.fn<(id: symbol, bottom: number | null) => void>(),
    }
    render(
      <FloatingContext value={boundary}>
        <StyleMenu
          styles={styleList(null)}
          current="natural"
          onPick={vi.fn<(id: string) => void>()}
          onEdit={vi.fn<() => void>()}
        />
      </FloatingContext>,
    )
    const chip = screen.getByTestId('tv-style-chip')
    fireEvent.click(chip, { detail: 0 })
    expect(chip).toHaveAttribute('aria-expanded', 'true')
    fireEvent.click(chip, { detail: 0 })
    expect(chip).toHaveAttribute('aria-expanded', 'false')
    fireEvent.keyDown(chip, { key: ' ' })
    expect(chip).toHaveAttribute('aria-expanded', 'true')
  })

  it('矮面板里 ⋯ 菜单收起低频项（「编辑当前风格」、不可用原因），关键项完整可见', async () => {
    const boundary = {
      container: null,
      rect: () => new DOMRect(0, 0, 400, 200),
      reserve: vi.fn<(id: symbol, bottom: number | null) => void>(),
    }
    render(
      <FloatingContext value={boundary}>
        <MoreMenu
          task="translate"
          refine={{ enabled: false, hint: '原文至少 40 词或 60 字' }}
          refined={null}
          models={[]}
          model={null}
          defaultModel="GPT-5.4 mini"
          onTask={vi.fn<(task: 'polish' | 'proofread') => void>()}
          onRefine={vi.fn<() => void>()}
          onToggleRefined={vi.fn<() => void>()}
          onCopySource={vi.fn<() => void>()}
          onModel={vi.fn<(ref: ModelRef | null) => void>()}
          onEditStyle={vi.fn<() => void>()}
        />
      </FloatingContext>,
    )
    fireEvent.click(screen.getByTestId('tv-more'))
    const menu = await screen.findByTestId('tv-more-menu')
    expect(screen.getByTestId('tv-more-menu-item-copy-source')).toBeInTheDocument()
    expect(screen.getByTestId('tv-more-menu-item-model')).toBeInTheDocument()
    expect(screen.queryByTestId('tv-more-menu-item-edit-style')).toBeNull()
    expect(menu.querySelector('.ui-menu-desc')).toBeNull()
  })

  it('不可用原因是该项的第二行说明（Menu 的 description，aria-describedby 关联）', async () => {
    await mount(payload())
    await finish('rx', '译文')
    fireEvent.click(screen.getByTestId('tv-more'))
    await screen.findByTestId('tv-more-menu')
    const item = screen.getByTestId('tv-more-menu-item-refine')
    const desc = item.querySelector('.ui-menu-desc')
    expect(desc).toHaveTextContent('原文至少 40 词或 60 字')
    expect(item.getAttribute('aria-describedby')).toBe(desc?.id)
  })
})

describe('反馈与状态', () => {
  it('⌘C 复制全文：复制按钮换成对勾，同时显示轻提示「已复制」', async () => {
    await mount(payload())
    await finish('rx', '敏捷的棕色狐狸')
    press('KeyC', { mod: true })
    await flush()
    expect(ipc.calls.findLast(([c]) => c === 'clipboard_copy_text')?.[1]).toEqual({
      text: '敏捷的棕色狐狸',
    })
    await waitFor(() => expect(screen.getByTestId('tv-flash')).toHaveTextContent('已复制'))
    expect(screen.getByTestId('tv-flash')).toHaveAttribute('data-tone', 'success')
    expect(screen.getByTestId('tv-copy').querySelector('[data-copied]')).not.toBeNull()
  })

  it('提升译文以截断结束、没有任何 Delta：保留原译文，状态行提示思考耗尽，不同时显示「无需改进」或空状态', async () => {
    await mount(payload({ source_text: 'word '.repeat(45) }))
    await finish('rx', '原译文')
    press('KeyR', { mod: true, shift: true })
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({ task: 'refine', draft: '原译文' })
    ipc.push('r1', start('r1'), done({ truncated: 'max_tokens' }))
    expect(bodyText()).toBe('原译文')
    expect(screen.getByTestId('tv-status')).toHaveTextContent('模型思考耗尽了输出额度')
    expect(screen.getByTestId('tv-status')).not.toHaveTextContent('无需改进')
    expect(screen.queryByTestId('tv-exhausted')).toBeNull()
    expect(screen.getByTestId('tv-pin')).toBeInTheDocument()
  })

  it('面板关闭后到达的 Error{cancelled} 不改变界面（淡出期间底栏不从 [停止] 跳成完成态）', async () => {
    const { rerender } = await mount(payload())
    ipc.push('rx', start('rx'), delta('x'))
    rerender({ phase: 'exiting' })
    ipc.push('rx', error('cancelled'))
    expect(screen.getByTestId('view-translate')).toHaveAttribute('data-status', 'streaming')
    expect(screen.getByTestId('tv-stop')).toBeInTheDocument()
    expect(screen.getByTestId('tv-status')).not.toHaveTextContent('已取消')
  })

  it('JIT 预热：在暂存节点里渲染一次再卸载，不发 IPC、不登记键盘作用域、不留节点', async () => {
    const scopes = activeScopes().length
    const calls = ipc.calls.length
    prewarmRender()
    await flush()
    expect(ipc.calls.length).toBe(calls)
    expect(document.querySelector('[data-testid="view-translate"]')).toBeNull()
    expect(activeScopes().length).toBe(scopes)
  })
})

describe('拖动', () => {
  const drags = () => ipc.calls.filter(([c]) => c === 'panel_drag_start')
  const down = (el: Element) => fireEvent.pointerDown(el, { button: 0, pointerType: 'mouse' })

  it('面板宿主：从头部与空白处按下即拖动（带会话 id），没有焦点时同时成为 key；按钮与正文不触发', async () => {
    const { props, container } = await mount(payload(), { focused: false })
    await finish('rx', '敏捷的棕色狐狸')
    const spacer = container.querySelector('.tv-header .tv-spacer')
    if (!spacer) throw new Error('no spacer')
    down(spacer)
    expect(drags()).toEqual([['panel_drag_start', { session_id: 3 }]])
    expect(props.activate).toHaveBeenCalledTimes(1)
    // 发起拖动的这次按下：mousedown 阻止默认行为（不开始选中、焦点不动）
    expect(fireEvent.mouseDown(spacer, { button: 0 })).toBe(false)
    down(screen.getByTestId('view-translate'))
    expect(drags()).toHaveLength(2)
    for (const el of [screen.getByTestId('tv-close'), screen.getByTestId('tv-stream-body')]) {
      down(el)
      expect(fireEvent.mouseDown(el, { button: 0 })).toBe(true)
    }
    expect(drags()).toHaveLength(2)
    // 面板宿主不用 data-tauri-drag-region（面板窗口的拖动经 panel_drag_start，Rust 跟随几何）
    expect(container.querySelector('[data-tauri-drag-region]')).toBeNull()
  })

  it('固定窗口宿主：不发 panel_drag_start（头部由 data-tauri-drag-region 拖动）', async () => {
    const { container } = await mount(payload(), { host: 'pinned' })
    const spacer = container.querySelector('.tv-header .tv-spacer')
    if (!spacer) throw new Error('no spacer')
    down(spacer)
    expect(drags()).toHaveLength(0)
  })
})
