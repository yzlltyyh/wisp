/**
 * 输入视图：自动聚焦与占位文案、⌘↩ 提交（IME 组合中不提交）、
 * 提交后 switchView 到翻译视图并带上 StartAck.width、手动改目标语言、长文本先确认、命令失败显示错误卡片、
 * NotReadyBanner 渲染 InputPayload.hint。
 */
import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { lastPrewarm } from '../../../shared/perf/prewarm'
import { boot, WispIpcError } from '../../../shared/ipc'
import type { NotReady, PanelPayload } from '../../../shared/ipc/types'
import type { ViewProps } from '../../../panel/types'
import { readHandoff } from '../../translate/handoff'
import { subscribeBuffered } from '../../translate/hub'
import {
  delta,
  flush,
  idleTransport,
  installFakeIpc,
  press,
  start,
  type FakeIpc,
} from '../../translate/__tests__/harness'
import { InputView, prewarmInputRender } from '../InputView'
import { activeScopes } from '../../../shared/keys/keyRouter'

let ipc: FakeIpc

beforeEach(() => {
  ipc = installFakeIpc()
})
afterEach(() => idleTransport())

function mount(
  initial = '',
  hint: NotReady | null = null,
  phase: ViewProps<'input'>['phase'] = 'shown',
) {
  const props = {
    sessionId: 5,
    payload: { kind: 'input', initial_text: initial, target_lang: null, hint } as Extract<
      PanelPayload,
      { kind: 'input' }
    >,
    phase,
    focused: true,
    host: 'panel' as const,
    frame: 'css' as const,
    placement: 'below' as const,
    maxHeight: 480,
    requestHide: vi.fn<ViewProps<'input'>['requestHide']>(),
    switchView: vi.fn<ViewProps<'input'>['switchView']>(),
    activate: vi.fn<ViewProps<'input'>['activate']>(),
  } satisfies ViewProps<'input'>
  const r = render(<InputView {...props} />)
  return Object.assign(props, {
    unmount: r.unmount,
    rerender: (patch: Partial<ViewProps<'input'>>) =>
      r.rerender(<InputView {...props} {...patch} />),
  })
}

const textarea = () => screen.getByTestId('iv-text') as HTMLTextAreaElement

describe('输入视图', () => {
  it('输入框自动聚焦，占位文案简短（快捷键已在按钮上）；空内容时翻译按钮禁用', () => {
    mount()
    expect(document.activeElement).toBe(textarea())
    expect(textarea().placeholder).toBe('输入或粘贴文字…')
    expect(screen.getByTestId('iv-submit')).toBeDisabled()
  })

  it('⌘R / ⌘⇧R / ⌘P 在输入视图里没有动作，也吞掉（不落到 WebView2 的重载 / 打印预览）', () => {
    mount('hello')
    textarea().focus()
    for (const [code, shift] of [
      ['KeyR', false],
      ['KeyR', true],
      ['KeyP', false],
    ] as const) {
      const event = new KeyboardEvent('keydown', {
        code,
        key: code.slice(3).toLowerCase(),
        metaKey: boot.os === 'macos',
        ctrlKey: boot.os !== 'macos',
        shiftKey: shift,
        bubbles: true,
        cancelable: true,
      })
      act(() => {
        textarea().dispatchEvent(event)
      })
      expect(event.defaultPrevented).toBe(true)
    }
    expect(ipc.starts).toHaveLength(0)
  })

  it('方向与 Rust 同一套规则：汉字 + 假名的日文显示「JA → 中文」；交换没有意义时禁用', () => {
    mount('日本語の文章です')
    expect(screen.getByTestId('iv-langbar-source')).toHaveTextContent('日语')
    expect(screen.getByTestId('iv-langbar-target')).toHaveTextContent('中文')
    // 日文 → 交换为英语（有意义）
    expect(screen.getByTestId('iv-langbar-source')).not.toHaveAttribute('aria-disabled')
    fireEvent.change(textarea(), { target: { value: 'hello world' } })
    // 英文原文：交换后目标也是英语，没有意义
    expect(screen.getByTestId('iv-langbar-source')).toHaveAttribute('aria-disabled', 'true')
  })

  it('⌘↩ 提交：发起翻译（会话 id、默认风格、自动方向），拿到 StartAck 后交叉淡入到翻译视图并带上新宽度', async () => {
    ipc.ackWidth = 380
    const props = mount('周五之前交付设计稿')
    expect(screen.getByTestId('iv-langbar')).toHaveTextContent('中文')
    expect(screen.getByTestId('iv-langbar')).toHaveTextContent('英语')
    press('Enter', { mod: true })
    await flush()
    expect(ipc.starts).toEqual([
      expect.objectContaining({
        session_id: 5,
        text: '周五之前交付设计稿',
        task: 'translate',
        style_id: 'natural',
        target_lang: null,
      }),
    ])
    expect(props.switchView).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: 'translate',
        source_text: '周五之前交付设计稿',
        request_id: 'r1',
        detected_lang: 'zh-CN',
        target_lang: 'en',
      }),
      { width: 380 },
    )
    // 事件进了 hub 的缓冲，翻译视图接上时同步回放，不再经 IPC
    ipc.push('r1', start('r1'), delta('Deliver'))
    const seen: string[] = []
    subscribeBuffered('r1', (e) => seen.push(e.type))
    expect(seen).toEqual(['start', 'delta'])
    // 重复提交无效
    press('Enter', { mod: true })
    await flush()
    expect(ipc.starts).toHaveLength(1)
  })

  it('IME 组合输入中的 ⌘↩ 不提交', async () => {
    mount('输入中')
    act(() => {
      textarea().dispatchEvent(
        new KeyboardEvent('keydown', {
          key: 'Enter',
          code: 'Enter',
          metaKey: true,
          ctrlKey: true,
          isComposing: true,
          bubbles: true,
        }),
      )
    })
    await flush()
    expect(ipc.starts).toHaveLength(0)
  })

  it('手动改目标语言只对本会话生效：随请求发出，并交接给翻译视图', async () => {
    const props = mount('hello world')
    fireEvent.click(screen.getByTestId('iv-langbar-target'))
    // 目标默认是中文（原文是英文）：菜单里选「英语」（源语言按钮现在也写全称「英语」，按菜单项定位）
    fireEvent.click(await screen.findByTestId('iv-langbar-menu-item-en'))
    fireEvent.click(screen.getByTestId('iv-submit'))
    await flush()
    expect(ipc.starts[0]?.target_lang).toBe('en')
    expect(readHandoff('r1')).toEqual({ target: 'en', forcedSource: null })
    expect(ipc.names()).not.toContain('settings_patch')
    expect(props.switchView).toHaveBeenCalled()
  })

  it('超过长文本阈值：Rust 拦下（StartAck.long_text）后切到翻译视图确认，不发请求', async () => {
    const long = 'word '.repeat(700)
    const props = mount(long)
    fireEvent.click(screen.getByTestId('iv-submit'))
    await flush()
    // 交给 Rust 判定（confirm_long = false），请求没有发出
    const sent = ipc.calls.filter(([c]) => c === 'translate_start')
    expect(sent).toHaveLength(1)
    const req = sent[0]?.[1].req as { confirm_long?: boolean } | undefined
    expect(req?.confirm_long).toBe(false)
    expect(ipc.starts).toHaveLength(0)
    expect(props.switchView).toHaveBeenCalledWith(
      expect.objectContaining({
        request_id: null,
        long_text: expect.objectContaining({ chars: long.length, est_tokens: expect.any(Number) }),
      }),
      { width: null },
    )
  })

  it('命令失败（还没有接入模型）：切到翻译视图，由它显示错误卡片', async () => {
    ipc.handlers.translate_start = () => {
      throw new WispIpcError('translate_start', {
        code: 'not_ready',
        message: 'no provider',
        llm: null,
      })
    }
    const props = mount('hello')
    fireEvent.click(screen.getByTestId('iv-submit'))
    await flush()
    const [next] = props.switchView.mock.calls[0] ?? []
    expect(next).toMatchObject({ kind: 'translate', request_id: expect.stringMatching(/^local-/) })
    const seen: string[] = []
    subscribeBuffered((next as { request_id: string }).request_id, (e) =>
      seen.push(e.type === 'error' ? e.code : e.type),
    )
    expect(seen).toEqual(['not_configured'])
  })

  it('StartAck 返回前面板已开始关闭 / 视图已卸载：不切换视图、取消这个请求、丢掉缓冲', async () => {
    let resolveAck: (v: unknown) => void = () => undefined
    ipc.handlers.translate_start = () =>
      new Promise((r) => {
        resolveAck = r
      })
    const props = mount('hello world')
    fireEvent.click(screen.getByTestId('iv-submit'))
    props.rerender({ phase: 'exiting' })
    resolveAck({ request_id: 'late', width: null })
    await flush()
    expect(props.switchView).not.toHaveBeenCalled()
    expect(ipc.calls.find(([c]) => c === 'cancel')?.[1]).toEqual({ request_id: 'late' })
    expect(subscribeBuffered('late', () => undefined)).toBeNull()

    let resolve2: (v: unknown) => void = () => undefined
    ipc.handlers.translate_start = () =>
      new Promise((r) => {
        resolve2 = r
      })
    props.unmount()
    const again = mount('second')
    fireEvent.click(screen.getByTestId('iv-submit'))
    again.unmount()
    resolve2({ request_id: 'gone', width: null })
    await flush()
    expect(again.switchView).not.toHaveBeenCalled()
    expect(ipc.calls.filter(([c]) => c === 'cancel').map(([, a]) => a.request_id)).toEqual([
      'late',
      'gone',
    ])
  })

  it('显示之后窗口获得焦点、焦点不在任何控件上：再聚焦一次输入框（第一次聚焦落空时）', () => {
    const props = mount('', null, 'preparing')
    textarea().blur()
    expect(document.activeElement).not.toBe(textarea())
    props.rerender({ phase: 'shown' })
    expect(document.activeElement).toBe(textarea())
  })

  it('取词失败的原因用 NotReadyBanner 显示；缺权限时「去授权」打开系统设置，可关闭', () => {
    mount('', { reason: 'permission_missing', permission: 'accessibility' })
    expect(screen.getByTestId('iv-hint')).toHaveTextContent('授予辅助功能权限后可直接读取选中文本')
    fireEvent.click(screen.getByTestId('iv-hint-action'))
    expect(ipc.calls.find(([c]) => c === 'permissions_open')?.[1]).toEqual({
      kind: 'accessibility',
    })
    fireEvent.click(screen.getByRole('button', { name: '关闭提示' }))
    expect(screen.queryByTestId('iv-hint')).toBeNull()
  })

  it('JIT 预热：在暂存节点里渲染一次再卸载，不发 IPC（不拉设置快照）、不登记键盘作用域、不抢焦点、不留节点', async () => {
    const scopes = activeScopes().length
    const calls = ipc.calls.length
    const focused = document.activeElement
    prewarmInputRender()
    // 真的渲染了一棵视图（预热会吞掉异常：渲染失败时测试要能发现）
    expect(lastPrewarm.error).toBeNull()
    expect(lastPrewarm.nodes).toBeGreaterThan(10)
    await act(async () => {
      await Promise.resolve()
    })
    expect(ipc.calls.length).toBe(calls)
    expect(document.querySelector('[data-testid="view-input"]')).toBeNull()
    expect(activeScopes().length).toBe(scopes)
    expect(document.activeElement).toBe(focused)
  })
})

describe('拖动', () => {
  const drags = () => ipc.calls.filter(([c]) => c === 'panel_drag_start')
  const down = (el: Element) => fireEvent.pointerDown(el, { button: 0, pointerType: 'mouse' })

  it('从头部与底栏的空白处拖动面板，焦点留在输入框；输入框、语言条、按钮不触发', () => {
    mount('hello')
    textarea().focus()
    const view = screen.getByTestId('view-input')
    const header = view.querySelector('.tv-header')
    const hint = view.querySelector('.iv-hint')
    if (!header || !hint) throw new Error('missing header / hint')
    down(header)
    expect(drags()).toEqual([['panel_drag_start', { session_id: 5 }]])
    expect(fireEvent.mouseDown(header, { button: 0 })).toBe(false)
    expect(document.activeElement).toBe(textarea())
    down(hint)
    expect(drags()).toHaveLength(2)
    for (const el of [
      textarea(),
      screen.getByTestId('iv-submit'),
      screen.getByTestId('iv-close'),
    ]) {
      down(el)
    }
    const langbar = screen.getByTestId('iv-langbar').querySelector('button')
    if (langbar) down(langbar)
    expect(drags()).toHaveLength(2)
  })
})
