/**
 * 剪贴板的操作菜单与指针（组件库的 PagedMenu / Menu 在真实视图里的效果）：
 * - 指针停在「粘贴为 ›」上进入子页；紧接着在原处点击（含 1–3 px 抖动）不粘贴、菜单仍开着，离开原处之后照常粘贴；
 * - 菜单开着时点另一行只关菜单、不粘贴它（右键菜单与 ⋯ 菜单都是）；再点一次才粘贴。
 */
import { act, fireEvent, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport } from '../../../shared/ipc'
import type { ClipInspect } from '../../../shared/ipc/types'
import { ms, timing } from '../../../shared/motion/tokens'
import { ClipboardView } from '../ClipboardView'
import { INSPECT_DEBOUNCE_MS } from '../model'
import { preloadPreview } from '../previewModule'
import { resetWebviewClipboardForTest } from '../useClipboard'
import {
  fakeIpc,
  idleTransport,
  meta,
  payload,
  renderIn,
  restorePlatform,
  setPlatform,
  viewProps,
  type FakeIpc,
} from './helpers'

let ipc: FakeIpc

const INSPECT: ClipInspect = { entities: [], transforms: ['trim', 'upper'] }

beforeEach(async () => {
  ipc = fakeIpc()
  ipc.results.clipboard_paste = { type: 'pasted' }
  ipc.results.clipboard_inspect = INSPECT
  setPlatform('macos')
  await preloadPreview()
})

afterEach(() => {
  restorePlatform()
  setTransport(idleTransport())
  resetWebviewClipboardForTest()
  vi.useRealTimers()
})

function mount() {
  const items = [meta(1, { preview: ' Hello ' }), meta(2, { preview: 'world' }), meta(3)]
  renderIn(<ClipboardView {...viewProps<'clipboard'>(payload(items))} />)
}

const row = (id: number) => document.getElementById(`clip-row-${id}`) as HTMLElement
const settleInspect = () =>
  act(() => new Promise<void>((r) => setTimeout(r, INSPECT_DEBOUNCE_MS + 20)))
const wait = (t: number) => act(() => new Promise<void>((r) => setTimeout(r, t)))
/** 当前页（换页时旧页还在淡出）里的菜单项 */
const current = (testId: string) =>
  document.querySelector<HTMLElement>(
    `[role="menu"] [data-page]:not([aria-hidden]) [data-testid="${testId}"]`,
  ) as HTMLElement
function move(el: Element, x: number, y: number) {
  fireEvent.pointerMove(el, { clientX: x, clientY: y, pointerType: 'mouse' })
}
/** 一次完整的指针点击（按下 → 松开 → click） */
function press(el: Element, x: number, y: number) {
  const init = { clientX: x, clientY: y, button: 0, pointerType: 'mouse' }
  fireEvent.pointerDown(el, init)
  fireEvent.mouseDown(el, init)
  fireEvent.pointerUp(el, init)
  fireEvent.mouseUp(el, init)
  fireEvent.click(el, init)
}

describe('操作菜单：悬停进入「粘贴为」', () => {
  it('停住进入子页；在原处点击（含抖动）不粘贴、菜单仍开着；离开原处之后照常粘贴', async () => {
    mount()
    await settleInspect()
    fireEvent.contextMenu(row(1), { clientX: 10, clientY: 10 })
    await screen.findByTestId('clip-context-menu')
    // 从「粘贴」移进「粘贴为 ›」（第一次移动只记基线）
    move(current('clip-context-menu-item-paste'), 40, 20)
    move(current('clip-context-menu-item-paste'), 40, 22)
    move(current('clip-context-menu-item-paste_as'), 40, 50)
    await wait(ms(timing.submenuDwell) + 60)
    expect(await screen.findByTestId('clip-menu-paste_as')).toBeTruthy()
    // 悬停进入的子页不预先高亮
    expect(screen.getByTestId('clip-menu-paste_as').hasAttribute('aria-activedescendant')).toBe(
      false,
    )
    press(current('clip-menu-paste_as-item-transform-trim'), 40, 50)
    move(current('clip-menu-paste_as-item-transform-trim'), 42, 51)
    press(current('clip-menu-paste_as-item-transform-trim'), 42, 51)
    move(current('clip-menu-paste_as-item-transform-upper'), 43, 53)
    press(current('clip-menu-paste_as-item-transform-upper'), 43, 53)
    expect(ipc.called('clipboard_paste')).toEqual([])
    expect(screen.getByTestId('clip-menu-paste_as')).toBeTruthy()
    // 子页变矮、指针落到下面的列表上：列表的光标不跟着走（否则菜单作用的那一条与子页内容会变）
    move(row(2), 44, 54)
    move(row(2), 45, 56)
    await wait(INSPECT_DEBOUNCE_MS + 40)
    expect(ipc.called('clipboard_inspect')).not.toContainEqual({ id: 2 })
    expect(current('clip-menu-paste_as-item-transform-upper')).toBeTruthy()
    // 离开原处（> 6 px）之后照常
    move(current('clip-menu-paste_as-item-transform-upper'), 40, 70)
    press(current('clip-menu-paste_as-item-transform-upper'), 40, 70)
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 1, mode: 'normal', transform: 'upper' }])
  })
})

describe('菜单开着时点另一行：只关菜单', () => {
  it('右键菜单开着时点另一行不粘贴；再点一次才粘贴', async () => {
    mount()
    await settleInspect()
    fireEvent.contextMenu(row(1), { clientX: 10, clientY: 10 })
    await screen.findByTestId('clip-context-menu')
    press(row(2), 10, 60)
    expect(ipc.called('clipboard_paste')).toEqual([])
    await wait(ms(timing.menuOut) + 50)
    expect(screen.queryByTestId('clip-context-menu')).toBeNull()
    press(row(2), 10, 60)
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 2, mode: 'normal' }])
  })

  it('⋯ 菜单开着时点一行不粘贴', async () => {
    mount()
    fireEvent.click(screen.getByTestId('clip-more'))
    await screen.findByTestId('clip-more-menu')
    press(row(2), 10, 60)
    expect(ipc.called('clipboard_paste')).toEqual([])
    expect(screen.getByTestId('clip-more')).toHaveAttribute('aria-expanded', 'false')
  })
})
