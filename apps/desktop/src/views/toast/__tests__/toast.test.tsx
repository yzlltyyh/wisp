/**
 * toast 视图：每个 CopiedReason 都有中英文文案；粘贴键按 ToastPayload.paste_key
 * 原样渲染；显示后约 1.6 s 前端请求隐藏（panel_hide(other)），指针停在上面时暂停；点击即关闭；macOS 缺少辅助功能
 * 权限时点按打开设置页；Wayland 前端退路下第一次按键就让出焦点；读屏通过先挂载的 live region 播报。
 */
import { act, fireEvent, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { CopiedReason } from '../../../shared/ipc/types'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import { t } from '../../../shared/i18n/i18n'
import { ms, timing } from '../../../shared/motion/tokens'
import { CLIPBOARD_MESSAGES } from '../../clipboard/i18n'
import { setTransport } from '../../../shared/ipc'
import {
  fakeIpc,
  idleTransport,
  key,
  renderIn,
  restorePlatform,
  setPlatform,
  viewProps,
} from '../../clipboard/__tests__/helpers'
import { toastHoldMs, ToastView } from '../ToastView'

const REASONS: CopiedReason[] = [
  'copy_only',
  'wayland',
  'elevated',
  'target_gone',
  'focus_timeout',
  'no_permission',
  'manual',
]
/** 带原因说明的（仅复制与 manual 没有说明） */
const WITH_DETAIL = REASONS.filter((r) => r !== 'copy_only' && r !== 'manual')

/** Rust 不激活地显示 toast（focused = false）；`focused: true` 是剪贴板视图在原位切过来的前端退路 */
function mount(reason: CopiedReason, extra = {}) {
  const props = viewProps<'toast'>(
    { kind: 'toast', reason, paste_key: '⌘V' },
    { focused: false, ...extra },
  )
  renderIn(<ToastView {...props} />)
  return props
}

beforeEach(() => setPlatform('macos'))
afterEach(() => {
  restorePlatform()
  setTransport(idleTransport())
  vi.useRealTimers()
})

describe('文案', () => {
  it('每个 CopiedReason 在 zh-CN 与 en-US 都有非空文案', () => {
    for (const locale of ['zh-CN', 'en-US'] as const) {
      const toast = CLIPBOARD_MESSAGES[locale].toast
      expect(toast.copied).toBeTruthy()
      expect(toast.press_to_paste).toContain('{key}')
      for (const reason of WITH_DETAIL) {
        expect(
          toast.reason[reason as keyof typeof toast.reason],
          `${locale} ${reason}`,
        ).toBeTruthy()
      }
      expect(toast.reason.no_permission_macos).toBeTruthy()
    }
  })

  it('仅复制：只有「已复制」一行', () => {
    mount('copy_only')
    expect(screen.getByTestId('toast-title')).toHaveTextContent(t('clipboard.toast.copied'))
    expect(screen.queryByTestId('toast-detail')).toBeNull()
  })

  it('manual：只有「已复制，按 ⌘V 粘贴」一行，不写原因', () => {
    mount('manual')
    expect(screen.getByTestId('toast-title').querySelector('kbd')?.textContent).toBe('⌘V')
    expect(screen.queryByTestId('toast-detail')).toBeNull()
  })

  it.each(WITH_DETAIL)('%s：「已复制，按 ⌘V 粘贴」+ 原因', (reason) => {
    mount(reason)
    const title = screen.getByTestId('toast-title')
    expect(title.querySelector('kbd')?.textContent).toBe('⌘V')
    expect(screen.getByTestId('toast-detail').textContent).toBeTruthy()
  })

  it('缺少权限：macOS 说明辅助功能，其他平台用通用说明', () => {
    mount('no_permission')
    expect(screen.getByTestId('toast-detail')).toHaveTextContent(
      t('clipboard.toast.reason.no_permission_macos'),
    )
  })

  it('英文界面', () => {
    setAppearance({ ...getAppearance(), uiLanguage: 'en-US' })
    mount('elevated')
    expect(screen.getByTestId('toast-detail')).toHaveTextContent(
      CLIPBOARD_MESSAGES['en-US'].toast.reason.elevated,
    )
  })
})

describe('停留与关闭', () => {
  it('显示后 1.6 s 请求隐藏（other）；准备阶段不计时', () => {
    vi.useFakeTimers()
    const props = mount('copy_only', { phase: 'preparing' })
    act(() => vi.advanceTimersByTime(ms(timing.toastViewHold) + 100))
    expect(props.requestHide).not.toHaveBeenCalled()
    const shown = mount('copy_only')
    act(() => vi.advanceTimersByTime(ms(timing.toastViewHold) - 10))
    expect(shown.requestHide).not.toHaveBeenCalled()
    act(() => vi.advanceTimersByTime(20))
    expect(shown.requestHide).toHaveBeenCalledWith('other')
  })

  it('带原因说明的 toast 停留更久（至少约 4 s、最多约 5 s，随文案长度）', () => {
    vi.useFakeTimers()
    const props = mount('target_gone')
    act(() => vi.advanceTimersByTime(ms(timing.toastViewDetailMin) - 20))
    expect(props.requestHide).not.toHaveBeenCalled()
    act(() => vi.advanceTimersByTime(40))
    expect(props.requestHide).toHaveBeenCalledWith('other')
    expect(toastHoldMs('x'.repeat(500), true, false)).toBe(ms(timing.toastViewDetailMax))
    expect(toastHoldMs('已复制', false, false)).toBe(ms(timing.toastViewHold))
  })

  it('指针停在上面时暂停计时，移开后重新计时；点击立即关闭', () => {
    vi.useFakeTimers()
    const props = mount('wayland')
    const dismiss = screen.getByTestId('toast-dismiss')
    fireEvent.pointerEnter(dismiss)
    act(() => vi.advanceTimersByTime(ms(timing.toastViewHold) * 5))
    expect(props.requestHide).not.toHaveBeenCalled()
    fireEvent.pointerLeave(dismiss)
    act(() => vi.advanceTimersByTime(ms(timing.toastViewDetailMax) + 10))
    expect(props.requestHide).toHaveBeenCalledTimes(1)
    fireEvent.click(dismiss)
    expect(props.requestHide).toHaveBeenCalledTimes(2)
  })
})

describe('macOS 缺少辅助功能权限', () => {
  it('说明 + 强调色的行动入口（写明去哪里开启）；点按打开「辅助功能」设置页并关闭 toast', async () => {
    const ipc = fakeIpc()
    const props = mount('no_permission')
    expect(screen.getByTestId('toast-detail')).toHaveTextContent(
      t('clipboard.toast.reason.no_permission_macos'),
    )
    expect(screen.getByTestId('toast-action')).toHaveTextContent(
      t('clipboard.toast.action.allow_accessibility'),
    )
    expect(screen.getByTestId('view-toast').hasAttribute('data-actionable')).toBe(true)
    const dismiss = screen.getByTestId('toast-dismiss')
    expect(dismiss.getAttribute('aria-label')).toBe(t('clipboard.toast.open_settings'))
    fireEvent.click(dismiss)
    await waitFor(() => expect(ipc.called('permissions_open')).toEqual([{ kind: 'accessibility' }]))
    expect(props.requestHide).toHaveBeenCalledWith('other')
  })
})

describe('前端退路（面板持有焦点：KDE Wayland 由剪贴板视图在原位切过来）', () => {
  it('主文案请用户在提示关闭后再按粘贴键；任何按键（单按 Ctrl 也算）都立即隐藏、把焦点还给原来的应用', () => {
    const props = mount('wayland', { focused: true })
    expect(screen.getByTestId('toast-title')).toHaveTextContent(
      t('clipboard.toast.press_after_close', { key: '⌘V' }),
    )
    key('ControlLeft', { key: 'Control', ctrl: true })
    expect(props.requestHide).toHaveBeenCalledWith('other')
  })

  it('停留时长不加长（用户在等它关掉）', () => {
    vi.useFakeTimers()
    const props = mount('wayland', { focused: true })
    act(() => vi.advanceTimersByTime(ms(timing.toastViewHold) + 20))
    expect(props.requestHide).toHaveBeenCalledWith('other')
  })

  it('仅复制（⌘↩）只显示「已复制」', () => {
    mount('copy_only', { focused: true })
    expect(screen.getByTestId('toast-title')).toHaveTextContent(t('clipboard.toast.copied'))
    expect(screen.queryByTestId('toast-detail')).toBeNull()
  })

  it('Rust 不激活地显示的 toast 收不到按键，也不响应', () => {
    const props = mount('copy_only')
    key('KeyV', { ctrl: true })
    expect(props.requestHide).not.toHaveBeenCalled()
  })
})

describe('读屏', () => {
  it('live region 先空着挂载，文案下一帧写入（会被播报）；外层没有 aria-label 盖住内容', async () => {
    mount('elevated')
    const live = screen.getByTestId('toast-live')
    expect(live.getAttribute('aria-live')).toBe('polite')
    expect(screen.getByTestId('view-toast').hasAttribute('aria-label')).toBe(false)
    await waitFor(() => expect(live.textContent).toContain(t('clipboard.toast.reason.elevated')))
  })
})
