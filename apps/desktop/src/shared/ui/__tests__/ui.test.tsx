/**
 * 组件库的交互与无障碍：菜单键盘导航（Esc 只关菜单不关面板）、错误卡片按错误码取舍按钮、
 * 快捷键录制器、TextArea 的 ⌘↩ 提交与输入法安全、自动增高（宽度 / 字号变化时重算）、
 * 暂存视图里的 autoFocus 推迟到视图显示、Tooltip 延迟、NumberField 夹取、浮层定位。
 */
import { act, cleanup as unmountAll, fireEvent, render, screen } from '@testing-library/react'
import * as React from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport, type Transport } from '../../ipc'
import type { ConflictReport, LlmError } from '../../ipc/types'
import { installKeyRouter, registerScope } from '../../keys/keyRouter'
import { MotionRoot } from '../../motion/MotionRoot'
import {
  computePosition,
  DropdownMenu,
  ErrorCard,
  HotkeyRecorder,
  NumberField,
  CopyButton,
  InlineConfirm,
  NotReadyBanner,
  NotReadyCard,
  TextArea,
  Thumbnail,
  Tooltip,
  VirtualList,
  type MenuEntry,
} from '..'
import { WispIpcError } from '../../ipc'
import { copyText } from '../../hooks/useCopy'
import { getAppearance, setAppearance } from '../../hooks/useAppearance'
import { createViewActivity, ViewActivityContext } from '../../hooks/useViewActive'
import { triggerResizeObservers } from '../../../test/stubs'

let calls: Array<[string, unknown]>

function transport(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>(cmd: string, args?: unknown) => {
      calls.push([cmd, args])
      return Promise.resolve(undefined as T)
    },
    openStream: () => Promise.reject(new Error('unused')),
    listen: () => Promise.resolve(() => {}),
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
}

beforeEach(() => {
  calls = []
  setTransport(transport())
})
afterEach(() => {
  // 不清成 null：迟到的调用（复制反馈计时器等）为 null 时会触发浏览器 mock 的异步加载，覆盖下一个用例的传输层
  setTransport(transport())
  vi.useRealTimers()
})

const wrap = (ui: React.ReactNode) => render(<MotionRoot>{ui}</MotionRoot>)

describe('DropdownMenu', () => {
  function setup() {
    const picked = vi.fn<(id: string) => void>()
    const closePanel = vi.fn<() => void>()
    const uninstall = installKeyRouter()
    const unregister = registerScope('panel', [{ combo: 'Escape', run: closePanel }], {
      layer: 'global',
    })
    const items: MenuEntry[] = [
      { id: 'natural', label: '自然', checked: true, onSelect: () => picked('natural') },
      { id: 'technical', label: '技术', checked: false, onSelect: () => picked('technical') },
      { type: 'separator', id: 's' },
      { id: 'disabled', label: '不可用', disabled: true },
      { id: 'learn', label: 'Learn', onSelect: () => picked('learn') },
    ]
    wrap(
      <DropdownMenu
        items={items}
        label="风格"
        renderTrigger={(p) => (
          <button type="button" {...p}>
            自然
          </button>
        )}
      />,
    )
    return { picked, closePanel, cleanup: () => (unregister(), uninstall()) }
  }

  it('↓ 打开并高亮第一项，↓↓ 跳过分隔线与禁用项，Enter 选择后关闭', async () => {
    const { picked, cleanup } = setup()
    const trigger = screen.getByRole('button', { name: '自然' })
    fireEvent.keyDown(trigger, { key: 'ArrowDown' })
    const menu = await screen.findByRole('menu')
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-0$/)
    fireEvent.keyDown(menu, { key: 'ArrowDown' })
    fireEvent.keyDown(menu, { key: 'ArrowDown' })
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-4$/)
    fireEvent.keyDown(menu, { key: 'Enter' })
    expect(picked).toHaveBeenCalledWith('learn')
    cleanup()
  })

  it('悬停只认真实的指针移动：坐标不变的合成 mousemove 不抢键盘高亮', async () => {
    const { cleanup } = setup()
    fireEvent.keyDown(screen.getByRole('button', { name: '自然' }), { key: 'ArrowDown' })
    const menu = await screen.findByRole('menu')
    const learn = screen.getByText('Learn')
    const technical = screen.getByText('技术')
    // 浮层出现在静止的指针下：第一次只记基线
    fireEvent.pointerMove(technical, { clientX: 30, clientY: 50 })
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-0$/)
    // 键盘把高亮移到 Learn；布局变化时 WebKit 合成的同坐标 mousemove 不算悬停
    fireEvent.keyDown(menu, { key: 'End' })
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-4$/)
    fireEvent.pointerMove(technical, { clientX: 30, clientY: 50 })
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-4$/)
    // 指针真的动了：照常悬停高亮
    fireEvent.pointerMove(learn, { clientX: 31, clientY: 90 })
    fireEvent.pointerMove(technical, { clientX: 32, clientY: 52 })
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-1$/)
    cleanup()
  })

  it('第一次打开：浮层定位之前（visibility: hidden）聚焦会落空，定位完成后焦点进入菜单', async () => {
    const { cleanup } = setup()
    // 模拟浏览器：不可见的元素拿不到焦点（jsdom 不检查可见性）
    const real = HTMLElement.prototype.focus
    const spy = vi.spyOn(HTMLElement.prototype, 'focus').mockImplementation(function (
      this: HTMLElement,
      opts?: FocusOptions,
    ) {
      if (this.style.visibility === 'hidden') return
      real.call(this, opts)
    })
    try {
      fireEvent.keyDown(screen.getByRole('button', { name: '自然' }), { key: 'ArrowDown' })
      const menu = await screen.findByRole('menu')
      expect(document.activeElement).toBe(menu)
    } finally {
      spy.mockRestore()
      cleanup()
    }
  })

  it('首字母跳转', async () => {
    const { cleanup } = setup()
    fireEvent.click(screen.getByRole('button', { name: '自然' }))
    const menu = await screen.findByRole('menu')
    fireEvent.keyDown(menu, { key: 'l' })
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-4$/)
    cleanup()
  })

  it('Esc 只关闭菜单，不会关闭面板（overlay 层先于全局层）', async () => {
    const { closePanel, cleanup } = setup()
    fireEvent.click(screen.getByRole('button', { name: '自然' }))
    await screen.findByRole('menu')
    fireEvent.keyDown(window, { key: 'Escape', code: 'Escape' })
    expect(closePanel).not.toHaveBeenCalled()
    expect(screen.getByRole('button', { name: '自然' }).getAttribute('aria-expanded')).toBe('false')
    cleanup()
  })
})

function llm(code: LlmError['code'], extra: Partial<LlmError> = {}): LlmError {
  return {
    code,
    subcode: null,
    message: 'm',
    retryable: true,
    hint: null,
    status: null,
    url: null,
    retry_after_s: null,
    upstream_request_id: null,
    ...extra,
  }
}

describe('ErrorCard', () => {
  const handlers = {
    onRetry: () => {},
    onOpenSettings: () => {},
    onChooseModel: () => {},
    onSwitchModel: () => {},
  }

  it('兼容端点：只有收到服务端响应的错误才用服务端原文当建议；network / timeout 照常显示本地化建议，原文放进详情', () => {
    wrap(
      <ErrorCard
        error={llm('network', { message: 'connection refused' })}
        preferServerMessage
        {...handlers}
        data-testid="net"
      />,
    )
    const net = screen.getByTestId('net')
    expect(net.querySelector('.ui-errorcard-suggestion')?.textContent).not.toBe(
      'connection refused',
    )
    fireEvent.click(screen.getByTestId('net-details-toggle'))
    expect(net.textContent).toContain('connection refused')
    wrap(
      <ErrorCard
        error={llm('auth', { message: 'invalid api key (relay)', status: 401 })}
        preferServerMessage
        {...handlers}
        data-testid="auth"
      />,
    )
    expect(screen.getByTestId('auth').querySelector('.ui-errorcard-suggestion')?.textContent).toBe(
      'invalid api key (relay)',
    )
    // 本地错误码即使带了状态码也不显示原文
    wrap(
      <ErrorCard
        error={llm('timeout', { message: 'deadline exceeded', status: 200 })}
        preferServerMessage
        {...handlers}
        data-testid="to"
      />,
    )
    expect(
      screen.getByTestId('to').querySelector('.ui-errorcard-suggestion')?.textContent,
    ).not.toBe('deadline exceeded')
  })

  it('NotReadyCard：屏幕录制卡片提供「重新启动」（macOS 的授权要重启才生效）', () => {
    const relaunch = vi.fn<() => void>()
    wrap(
      <NotReadyCard
        notReady={{ reason: 'permission_missing', permission: 'screen_recording' }}
        onOpenSystemSettings={() => {}}
        onRelaunch={relaunch}
        data-testid="sr"
      />,
    )
    fireEvent.click(screen.getByTestId('sr-relaunch'))
    expect(relaunch).toHaveBeenCalledTimes(1)
    expect(screen.getByTestId('sr-system')).toBeTruthy()
  })

  it('cancelled 与 param_unsupported 不出卡片', () => {
    const { container } = wrap(
      <>
        <ErrorCard error={llm('cancelled')} {...handlers} />
        <ErrorCard error={llm('param_unsupported')} {...handlers} />
      </>,
    )
    expect(container.querySelector('.ui-errorcard')).toBeNull()
  })

  it('quota 不提供重试；auth 只有打开设置；no_vision 有选择模型和打开设置', () => {
    wrap(<ErrorCard error={llm('quota')} {...handlers} data-testid="quota" />)
    expect(screen.queryByTestId('quota-retry')).toBeNull()
    expect(screen.getByTestId('quota-open_settings')).toBeTruthy()
    wrap(<ErrorCard error={llm('no_vision')} {...handlers} data-testid="nv" />)
    expect(screen.getByTestId('nv-choose_model')).toBeTruthy()
    expect(screen.getByTestId('nv-open_settings')).toBeTruthy()
  })

  it('rate_limited 倒计时：归零前重试按钮禁用', () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval', 'performance'] })
    wrap(
      <ErrorCard
        error={llm('rate_limited', { retry_after_s: 3 })}
        {...handlers}
        data-testid="rl"
      />,
    )
    const retry = screen.getByTestId('rl-retry') as HTMLButtonElement
    expect(retry.disabled).toBe(true)
    act(() => vi.advanceTimersByTime(3_100))
    expect((screen.getByTestId('rl-retry') as HTMLButtonElement).disabled).toBe(false)
  })

  it('详情可展开，包含请求 id / 状态 / URL', () => {
    wrap(
      <ErrorCard
        error={llm('upstream', {
          status: 503,
          upstream_request_id: 'req_1',
          url: 'api.x.com/v1/chat',
        })}
        {...handlers}
        data-testid="up"
      />,
    )
    fireEvent.click(screen.getByTestId('up-details-toggle'))
    expect(screen.getByText('req_1')).toBeTruthy()
    expect(screen.getByText('api.x.com/v1/chat')).toBeTruthy()
  })
})

describe('未就绪：NotReadyCard 与输入视图的 NotReadyBanner', () => {
  it('capture_failed 的 message 是 i18n key，缺文案时显示通用说明而不是原文', () => {
    wrap(
      <NotReadyCard
        notReady={{ reason: 'capture_failed', message: 'errors.capture.no_displays' }}
        data-testid="known"
      />,
    )
    const known = screen.getByTestId('known').textContent ?? ''
    expect(known).not.toContain('errors.capture')
    wrap(
      <NotReadyCard
        notReady={{ reason: 'capture_failed', message: 'backend: X11 capture timed out' }}
        data-testid="raw"
      />,
    )
    expect(screen.getByTestId('raw').textContent).not.toContain('X11 capture timed out')
  })

  it('取词失败只给提示条：缺权限时有「去授权」，其他原因没有按钮；不再提供「改用输入模式」', () => {
    const grant = vi.fn<(permission: string) => void>()
    wrap(
      <NotReadyBanner
        notReady={{ reason: 'permission_missing', permission: 'accessibility' }}
        onOpenSystemSettings={grant}
        data-testid="perm"
      />,
    )
    fireEvent.click(screen.getByTestId('perm-action'))
    expect(grant).toHaveBeenCalledWith('accessibility')
    wrap(<NotReadyBanner notReady={{ reason: 'elevated' }} data-testid="elev" />)
    expect(screen.queryByTestId('elev-action')).toBeNull()
    wrap(
      <NotReadyBanner
        notReady={{ reason: 'selection_unavailable', reason_key: 'errors.platform.no_such_key' }}
        data-testid="unknown"
      />,
    )
    expect(screen.getByTestId('unknown').textContent).not.toContain('errors.platform')
    wrap(
      <NotReadyCard
        notReady={{ reason: 'elevated' }}
        onOpenSettings={() => {}}
        data-testid="card"
      />,
    )
    expect(screen.getByTestId('card').querySelector('button')).toBeNull()
  })
})

describe('HotkeyRecorder', () => {
  function setup(report: Partial<ConflictReport> = {}) {
    const onChange = vi.fn<(combo: string | null, force: boolean) => void>()
    const check = vi.fn<(t: unknown, combo: string) => Promise<ConflictReport>>(
      async (_t, combo) => ({
        combo,
        display: combo,
        conflicts: [],
        acceptable: true,
        ...report,
      }),
    )
    wrap(
      <HotkeyRecorder
        target={{ kind: 'action', action: 'translate_selection' }}
        value="Alt+KeyD"
        onChange={onChange}
        check={check}
        label="划词翻译的快捷键"
      />,
    )
    const button = screen.getByRole('button', { name: /^划词翻译的快捷键/ })
    return { onChange, check, button }
  }

  it('无障碍名称带上当前组合的朗读文本（键帽被 aria-label 覆盖，读屏要能读出绑定）', () => {
    const { button } = setup()
    const name = button.getAttribute('aria-label') ?? ''
    expect(name.startsWith('划词翻译的快捷键')).toBe(true)
    expect(name).toMatch(/Option D$|Alt D$/)
  })

  /** 按下并抬起一个主键（修饰键状态在按下与抬起时相同） */
  async function press(button: HTMLElement, init: KeyboardEventInit) {
    await act(async () => {
      fireEvent.keyDown(button, init)
      fireEvent.keyUp(button, init)
    })
  }

  it('点击进入录制（录制期间暂停全局键），按下组合后检测冲突并确认', async () => {
    const { onChange, check, button } = setup()
    fireEvent.click(button)
    expect(calls.map(([c]) => c)).toContain('hotkey_record_begin')
    expect(button.getAttribute('aria-pressed')).toBe('true')
    await act(async () => {
      fireEvent.keyDown(button, { key: 'Alt', code: 'AltLeft', altKey: true })
      fireEvent.keyDown(button, { key: 'k', code: 'KeyK', altKey: true, shiftKey: true })
    })
    // 按下主键只回显候选组合，抬起主键才完成
    expect(check).not.toHaveBeenCalled()
    expect(button.getAttribute('aria-pressed')).toBe('true')
    await act(async () => {
      fireEvent.keyUp(button, { key: 'k', code: 'KeyK', altKey: true, shiftKey: true })
    })
    expect(check).toHaveBeenCalledWith(expect.anything(), 'Alt+Shift+KeyK')
    expect(onChange).toHaveBeenCalledWith('Alt+Shift+KeyK', false)
    expect(calls.map(([c]) => c)).toContain('hotkey_record_end')
  })

  it('macOS 按住 ⌘ 时主键没有 keyup：抬起修饰键也完成录制', async () => {
    const { onChange, check, button } = setup()
    fireEvent.click(button)
    await act(async () => {
      fireEvent.keyDown(button, { key: 'Meta', code: 'MetaLeft', metaKey: true })
      fireEvent.keyDown(button, { key: 'j', code: 'KeyJ', metaKey: true, shiftKey: true })
      fireEvent.keyUp(button, { key: 'Meta', code: 'MetaLeft', shiftKey: true })
    })
    expect(check).toHaveBeenCalledWith(expect.anything(), 'Shift+Super+KeyJ')
    expect(onChange).toHaveBeenCalledWith('Shift+Super+KeyJ', false)
  })

  it('录制中直接卸载（没有 blur）也会调用 hotkey_record_end', () => {
    const { button } = setup()
    fireEvent.click(button)
    expect(calls.map(([c]) => c)).toContain('hotkey_record_begin')
    expect(calls.map(([c]) => c)).not.toContain('hotkey_record_end')
    unmountAll()
    expect(calls.map(([c]) => c)).toContain('hotkey_record_end')
  })

  it('冲突检测失败时显示错误，不改动绑定', async () => {
    const onChange = vi.fn<(combo: string | null, force: boolean) => void>()
    wrap(
      <HotkeyRecorder
        target={{ kind: 'action', action: 'translate_selection' }}
        value="Alt+KeyD"
        onChange={onChange}
        check={() => Promise.reject(new Error('ipc down'))}
        label="划词翻译的快捷键"
      />,
    )
    const button = screen.getByRole('button', { name: /^划词翻译的快捷键/ })
    fireEvent.click(button)
    await press(button, { key: 'k', code: 'KeyK', altKey: true })
    expect(onChange).not.toHaveBeenCalled()
    expect(screen.getByText(/无法检测冲突|Couldn't check for conflicts/)).toBeTruthy()
  })

  it('Esc 取消、⌫ 清除；裸键需要修饰键', async () => {
    const { onChange, button } = setup()
    fireEvent.click(button)
    fireEvent.keyDown(button, { key: 'a', code: 'KeyA' })
    expect(onChange).not.toHaveBeenCalled()
    fireEvent.keyDown(button, { key: 'Escape', code: 'Escape' })
    expect(button.getAttribute('aria-pressed')).toBe('false')
    fireEvent.click(button)
    fireEvent.keyDown(button, { key: 'Backspace', code: 'Backspace' })
    expect(onChange).toHaveBeenCalledWith(null, false)
  })

  it('黄色冲突不自动确认，提供「仍然使用」', async () => {
    const { onChange, button } = setup({
      acceptable: true,
      conflicts: [
        {
          level: 'warning',
          source: 'neighbor',
          message_key: 'hotkeys.conflict.neighbor',
          params: { app: 'Easydict', combo: '⌥A' },
        },
      ],
    })
    fireEvent.click(button)
    await press(button, { key: 'a', code: 'KeyA', altKey: true })
    expect(onChange).not.toHaveBeenCalled()
    fireEvent.click(screen.getByRole('button', { name: /仍然使用|Use anyway/ }))
    expect(onChange).toHaveBeenCalledWith('Alt+KeyA', true)
  })
})

describe('TextArea · NumberField · Tooltip', () => {
  it('⌘↩ / Ctrl+Enter 提交；输入法组合中不提交；↩ 照常换行', () => {
    const onSubmit = vi.fn<() => void>()
    wrap(<TextArea value="x" onChange={() => {}} onSubmit={onSubmit} aria-label="input" />)
    const area = screen.getByLabelText('input')
    fireEvent.keyDown(area, { key: 'Enter', ctrlKey: true, isComposing: true })
    fireEvent.keyDown(area, { key: 'Enter' })
    expect(onSubmit).not.toHaveBeenCalled()
    fireEvent.keyDown(area, { key: 'Enter', ctrlKey: true })
    expect(onSubmit).toHaveBeenCalledTimes(1)
  })

  describe('TextArea 自动增高', () => {
    /** jsdom 没有布局：内容高度按「宽度 × 字号」折算（窄、字大 → 行多） */
    function layout() {
      const box = { width: 300 }
      vi.spyOn(HTMLTextAreaElement.prototype, 'clientWidth', 'get').mockImplementation(
        () => box.width,
      )
      vi.spyOn(HTMLTextAreaElement.prototype, 'scrollHeight', 'get').mockImplementation(
        () => Math.ceil((600 * getAppearance().fontSize) / 14 / box.width) * 20,
      )
      const frames: FrameRequestCallback[] = []
      vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => frames.push(cb))
      vi.stubGlobal('cancelAnimationFrame', () => {})
      const nextFrame = () => {
        for (const cb of frames.splice(0)) cb(performance.now())
      }
      return { box, frames, nextFrame }
    }
    const style = { lineHeight: '20px', padding: '0px', borderWidth: '0px' }
    const saved = getAppearance()
    afterEach(() => setAppearance(saved))

    it('宽度变化（视图切换改宽、固定窗口缩放）后在下一帧按新折行重算；只改高度不触发', () => {
      const { box, frames, nextFrame } = layout()
      wrap(<TextArea value="x" onChange={() => {}} minRows={1} maxRows={20} style={style} />)
      const area = screen.getByTestId('text-area')
      expect(area.style.height).toBe('40px')
      // 自己改高度引起的回调：宽度没变，不排队
      act(() => triggerResizeObservers())
      expect(frames).toHaveLength(0)
      box.width = 150
      act(() => {
        triggerResizeObservers()
        triggerResizeObservers()
      })
      // 同一帧里的多次变化合并成一个 rAF
      expect(frames).toHaveLength(1)
      act(() => nextFrame())
      expect(area.style.height).toBe('80px')
    })

    it('挂载在暂存层里：挂载的提交里不量高度（不强制排版暂存视图），视图显示的那次提交里量', () => {
      layout()
      const activity = createViewActivity(false)
      wrap(
        <ViewActivityContext value={activity}>
          <TextArea value="x" onChange={() => {}} minRows={1} maxRows={20} style={style} />
        </ViewActivityContext>,
      )
      const area = screen.getByTestId('text-area')
      expect(area.style.height).toBe('')
      act(() => activity.set(true))
      expect(area.style.height).toBe('40px')
    })

    it('正文字号变化（appearance:changed）后立即重算', () => {
      layout()
      wrap(<TextArea value="x" onChange={() => {}} minRows={1} maxRows={20} style={style} />)
      const area = screen.getByTestId('text-area')
      expect(area.style.height).toBe('40px')
      act(() => setAppearance({ ...getAppearance(), fontSize: 18 }))
      expect(area.style.height).toBe('60px')
    })
  })

  it('autoFocus：所在视图在暂存层时不聚焦，视图显示时再聚焦（只一次）', () => {
    const activity = createViewActivity(false)
    function Host({ value }: { value: string }) {
      return (
        <ViewActivityContext value={activity}>
          {/* oxlint-disable-next-line jsx-a11y/no-autofocus -- 输入 / 剪贴板视图的输入框自动聚焦，这里验证暂存视图推迟聚焦 */}
          <TextArea value={value} onChange={() => {}} autoFocus aria-label="draft" />
        </ViewActivityContext>
      )
    }
    const view = wrap(<Host value="" />)
    const area = screen.getByLabelText('draft')
    expect(document.activeElement).not.toBe(area)
    act(() => activity.set(true))
    expect(document.activeElement).toBe(area)
    // 之后失焦、视图再次不活动又活动，都不会被再次抢回
    area.blur()
    act(() => {
      activity.set(false)
      activity.set(true)
    })
    view.rerender(
      <MotionRoot>
        <Host value="a" />
      </MotionRoot>,
    )
    expect(document.activeElement).not.toBe(area)
  })

  it('NumberField：失焦时夹到范围内，↑↓ 步进', () => {
    const onChange = vi.fn<(n: number) => void>()
    wrap(<NumberField value={14} onChange={onChange} min={12} max={18} aria-label="字号" />)
    const input = screen.getByLabelText('字号')
    fireEvent.change(input, { target: { value: '30' } })
    fireEvent.blur(input)
    expect(onChange).toHaveBeenLastCalledWith(18)
    fireEvent.keyDown(input, { key: 'ArrowDown' })
    expect(onChange).toHaveBeenLastCalledWith(13)
  })

  it('Tooltip：悬停 500 ms 后出现，按键即消失', () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    wrap(
      <Tooltip content="复制" shortcut="Mod+KeyC">
        <button type="button">c</button>
      </Tooltip>,
    )
    const anchor = screen.getByText('c').parentElement as HTMLElement
    fireEvent.pointerEnter(anchor, { pointerType: 'mouse' })
    act(() => vi.advanceTimersByTime(400))
    expect(screen.queryByRole('tooltip')).toBeNull()
    act(() => vi.advanceTimersByTime(150))
    expect(screen.getByRole('tooltip').textContent).toContain('复制')
  })
})

describe('computePosition', () => {
  const boundary = { left: 0, top: 0, width: 400, height: 300 }
  it('下方放不下时翻到上方，水平夹在边界内，原点在触发器一侧', () => {
    const p = computePosition(
      { left: 380, top: 250, width: 20, height: 20 },
      { width: 160, height: 120 },
      boundary,
    )
    expect(p.side).toBe('top')
    expect(p.x).toBe(400 - 8 - 160)
    expect(p.origin.endsWith('bottom')).toBe(true)
  })
  it('上下都放不下时取空间大的一侧并限制最大高度', () => {
    const p = computePosition(
      { left: 10, top: 40, width: 20, height: 20 },
      { width: 100, height: 400 },
      boundary,
    )
    expect(p.side).toBe('bottom')
    expect(p.maxHeight).toBe(300 - 8 - 64)
  })
  it('shift（以指针为锚的右键菜单）：两侧都放不下而边界放得下时，整份上移到边界内、不限高', () => {
    // 锚点在 150：下方 300 − 8 − 154 = 138、上方 150 − 4 − 8 = 138，菜单 250 两侧都放不下，边界 284 放得下
    const anchor = { left: 50, top: 150, width: 0, height: 0 }
    const p = computePosition(anchor, { width: 200, height: 250 }, boundary, { shift: true })
    expect(p.maxHeight).toBe(250)
    expect(p.y).toBe(300 - 8 - 250)
    expect(p.bottomFromTop).toBe(300)
    // 不 shift：照旧限高滚动
    expect(computePosition(anchor, { width: 200, height: 250 }, boundary).maxHeight).toBe(138)
    // 边界本身比菜单矮：限高滚动
    expect(
      computePosition(anchor, { width: 200, height: 400 }, boundary, { shift: true }).maxHeight,
    ).toBeLessThan(400)
  })
})

describe('VirtualList', () => {
  it('只有首屏前 8 行用 Motion 做 stagger，其余行是普通 div；重渲染不会重新挂载行内容', () => {
    const saved = getAppearance()
    setAppearance({ ...saved, reduceMotion: false })
    // jsdom 没有布局：滚动容器的视口取 offsetWidth / offsetHeight；virtualizer 量到视口后的再渲染在 jsdom 里不会发生，
    // 首屏只有第 0 行 + overscan，所以把 overscan 调到 12
    vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockReturnValue(200)
    vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(300)
    const mounts = new Map<number, number>()
    function Row({ index }: { index: number }) {
      const [id] = React.useState(() => {
        mounts.set(index, (mounts.get(index) ?? 0) + 1)
        return index
      })
      return <span data-testid={`row-${id}`}>行 {id}</span>
    }
    const view = (selected: number) => (
      <VirtualList
        count={40}
        estimateSize={() => 20}
        renderItem={(i) => <Row index={i} />}
        height={200}
        stagger
        overscan={12}
        selectedIndex={selected}
      />
    )
    const { rerender } = wrap(view(0))
    const wrapper = (i: number) => screen.getByTestId(`row-${i}`).parentElement as HTMLElement
    // Motion 的行带进入动画的内联样式；普通行没有
    expect(wrapper(0).getAttribute('style')).toBeTruthy()
    expect(wrapper(7).getAttribute('style')).toBeTruthy()
    expect(wrapper(8).getAttribute('style')).toBeNull()
    rerender(<MotionRoot>{view(3)}</MotionRoot>)
    rerender(<MotionRoot>{view(5)}</MotionRoot>)
    for (const i of [0, 7, 8]) expect(mounts.get(i), `row ${i}`).toBe(1)
    setAppearance(saved)
  })
})

describe('Thumbnail', () => {
  it('换了 src 后重新进入 loading：之前的错误图标不残留，新图照样渲染', () => {
    const { rerender, container } = wrap(<Thumbnail src="a.png" alt="截图" />)
    const img = () => container.querySelector('img')
    fireEvent.error(img() as HTMLImageElement)
    expect(img()).toBeNull()
    expect(container.querySelector('.ui-thumb')?.getAttribute('data-state')).toBe('error')
    rerender(
      <MotionRoot>
        <Thumbnail src="b.png" alt="截图" />
      </MotionRoot>,
    )
    expect(img()?.getAttribute('src')).toBe('b.png')
    expect(container.querySelector('.ui-thumb')?.getAttribute('data-state')).toBe('loading')
    fireEvent.load(img() as HTMLImageElement)
    expect(container.querySelector('.ui-thumb')?.getAttribute('data-state')).toBe('loaded')
    rerender(
      <MotionRoot>
        <Thumbnail src="c.png" alt="截图" />
      </MotionRoot>,
    )
    // 新图同样从 loading 开始（加载后淡入）
    expect(container.querySelector('.ui-thumb')?.getAttribute('data-state')).toBe('loading')
  })
})

describe('InlineConfirm', () => {
  it('父组件频繁重渲染（每次传新的 onCancel）不会推迟 2 s 自动取消', () => {
    vi.useFakeTimers()
    const cancelled = vi.fn<(n: number) => void>()
    const view = (n: number) => (
      <MotionRoot>
        <InlineConfirm
          open
          message={`第 ${n} 次`}
          onConfirm={() => {}}
          onCancel={() => cancelled(n)}
        />
      </MotionRoot>
    )
    const { rerender } = render(view(0))
    for (let i = 1; i <= 10; i++) {
      act(() => vi.advanceTimersByTime(300))
      rerender(view(i))
    }
    // 已过去 3 s：自动取消只触发一次，用的是最新的回调
    expect(cancelled).toHaveBeenCalledTimes(1)
    expect(cancelled.mock.calls[0]?.[0]).toBeGreaterThanOrEqual(6)
  })
})

describe('copyText', () => {
  function failing(
    code: 'invalid_argument' | 'not_implemented' | 'internal' | 'platform',
    message: string = code,
  ) {
    setTransport({
      ...transport(),
      invoke: <T,>(cmd: string) =>
        Promise.reject(new WispIpcError(cmd, { code, message, llm: null })) as Promise<T>,
    })
  }

  it('平台层要求由 WebView 写入（KDE Wayland）时退回 navigator.clipboard，其他平台错误不退回', async () => {
    const write = vi.fn<(text: string) => Promise<void>>(() => Promise.resolve())
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText: write } })
    failing('platform', 'errors.platform.wayland_clipboard_bridge')
    expect(await copyText('a')).toBe(true)
    failing('platform', 'errors.platform.clipboard_too_large')
    expect(await copyText('b')).toBe(false)
    expect(write).toHaveBeenCalledTimes(1)
  })

  it('Rust 明确拒绝（invalid_argument：超过 1 MB）时不退回 navigator.clipboard', async () => {
    const write = vi.fn<(text: string) => Promise<void>>(() => Promise.resolve())
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText: write } })
    failing('invalid_argument')
    expect(await copyText('x'.repeat(10))).toBe(false)
    expect(write).not.toHaveBeenCalled()
  })

  it('传输层不可用 / 内部错误时退回 navigator.clipboard', async () => {
    const write = vi.fn<(text: string) => Promise<void>>(() => Promise.resolve())
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText: write } })
    failing('not_implemented')
    expect(await copyText('a')).toBe(true)
    failing('internal')
    expect(await copyText('b')).toBe(true)
    expect(write).toHaveBeenCalledTimes(2)
  })

  it('CopyButton 复制失败时显示失败状态并播报', async () => {
    failing('invalid_argument')
    const onCopied = vi.fn<(ok: boolean) => void>()
    wrap(<CopyButton text="x" onCopied={onCopied} />)
    await act(async () => {
      fireEvent.click(screen.getByTestId('copy-button'))
    })
    expect(onCopied).toHaveBeenCalledWith(false)
    expect(screen.getByText(/复制失败|Copy failed/)).toBeTruthy()
  })
})
