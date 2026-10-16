/**
 * 悬浮剪贴板的窗口外壳：
 * - 先订阅 `clipboard:changed` / `clipboard:queue`，再 `clipboard_float_ready`；视图渲染首帧之后 `clipboard_float_reveal`；
 * - 继承的查询 / 筛选 / 光标与置顶交给视图；置顶开关写回 Rust、失败时回到原状态；
 * - ⌘W 关闭（销毁窗口）；指针移入时请求同步拉取；取不到初始状态时留一个可以关闭的空卡片。
 */
import { act, fireEvent, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { boot, setTransport } from '../../shared/ipc'
import type { ClipFloatInit } from '../../shared/ipc/types'
import {
  fakeIpc,
  idleTransport,
  key,
  payload,
  renderIn,
  restorePlatform,
  sample,
  setPlatform,
  type FakeIpc,
} from '../../views/clipboard/__tests__/helpers'
import { FloatingApp } from '../FloatingApp'

let ipc: FakeIpc
const savedLabel = boot.label

function init(patch: Partial<ClipFloatInit> = {}): ClipFloatInit {
  const { kind: _kind, ...p } = payload(sample())
  return { query: '', filter: 'all', selected_id: 4, on_top: true, payload: p, ...patch }
}

beforeEach(() => {
  ipc = fakeIpc()
  setPlatform('macos')
  boot.label = 'clipfloat'
})
afterEach(() => {
  boot.label = savedLabel
  setTransport(idleTransport())
  restorePlatform()
})

describe('悬浮剪贴板窗口', () => {
  it('先订阅事件再 ready；首帧渲染之后 reveal（只一次），光标接着面板的那一条', async () => {
    let subscribedFirst = false
    ipc.results.clipboard_float_ready = () => {
      subscribedFirst =
        (ipc.listeners.get('clipboard:changed')?.size ?? 0) > 0 &&
        (ipc.listeners.get('clipboard:queue')?.size ?? 0) > 0
      return init()
    }
    renderIn(<FloatingApp />)
    await screen.findByTestId('view-clipboard')
    expect(subscribedFirst).toBe(true)
    await waitFor(() => expect(ipc.called('clipboard_float_reveal')).toHaveLength(1))
    const names = ipc.calls.map(([c]) => c)
    expect(names.indexOf('clipboard_float_ready')).toBeLessThan(
      names.indexOf('clipboard_float_reveal'),
    )
    expect(document.querySelector('[data-selected]')?.getAttribute('data-clip-id')).toBe('4')
    // 实时改动照常进到视图
    act(() =>
      ipc.emit('clipboard:changed', {
        change: 'removed',
        id: 4,
      }),
    )
    expect(document.getElementById('clip-row-4')).toBeNull()
  })

  it('置顶开关写回 Rust，失败时回到原状态；⌘W 关闭；指针移入请求同步', async () => {
    ipc.results.clipboard_float_ready = init({ on_top: true })
    ipc.results.clipboard_float_set_on_top = () => {
      throw { code: 'not_found', message: 'floating clipboard' }
    }
    renderIn(<FloatingApp />)
    const toggle = await screen.findByTestId('clip-tool-float_on_top')
    fireEvent.click(toggle)
    expect(ipc.called('clipboard_float_set_on_top')).toEqual([{ on_top: false }])
    await waitFor(() =>
      expect(screen.getByTestId('clip-tool-float_on_top').getAttribute('aria-pressed')).toBe(
        'true',
      ),
    )
    key('KeyW', { meta: true })
    expect(ipc.called('clipboard_float_close')).toHaveLength(1)
    fireEvent.pointerEnter(screen.getByTestId('floating-app'))
    expect(ipc.called('clipboard_float_hover')).toHaveLength(1)
  })

  it('取不到初始状态：一个可以关闭的空卡片', async () => {
    ipc.results.clipboard_float_ready = () => {
      throw { code: 'internal', message: 'gone' }
    }
    renderIn(<FloatingApp />)
    await screen.findByTestId('floating-app')
    expect(screen.queryByTestId('view-clipboard')).toBeNull()
    fireEvent.click(screen.getByRole('button'))
    expect(ipc.called('clipboard_float_close')).toHaveLength(1)
  })
})
