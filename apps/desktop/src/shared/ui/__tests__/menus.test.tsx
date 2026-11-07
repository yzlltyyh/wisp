/**
 * 菜单的滚动（jsdom 不排版：需要几何的用例把尺寸写进 getter）：
 * - useFloating：浮层自己的滚动不重新定位；测量自然尺寸时不改样式（不把 max-height 写成 none——那一刻菜单不再溢出，
 *   引擎会把滚动位置夹回 0）；
 * - Menu：悬停改变高亮不滚动菜单；键盘导航只滚菜单自己（不调用会连带滚动卡片的 scrollIntoView）；
 * - PagedMenu：换页之后重新预留卡片高度。
 */
import { act, fireEvent, render, screen } from '@testing-library/react'
import * as React from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport, type Transport } from '../../ipc'
import { MotionRoot } from '../../motion/MotionRoot'
import { DropdownMenu, PagedMenu, type MenuEntry, type MenuPage } from '..'
import { FloatingContext, type FloatingBoundary } from '../floating'

function transport(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>() => Promise.resolve(undefined as T),
    openStream: () => Promise.reject(new Error('unused')),
    listen: () => Promise.resolve(() => {}),
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
}

beforeEach(() => setTransport(transport()))
afterEach(() => vi.useRealTimers())

const wrap = (ui: React.ReactNode) => render(<MotionRoot>{ui}</MotionRoot>)

/** 等一帧（真实计时器下 jsdom 的 requestAnimationFrame） */
const frame = () =>
  act(
    () =>
      new Promise<void>((resolve) => {
        requestAnimationFrame(() => resolve())
      }),
  )

function items(n: number, pick: (id: string) => void = () => {}): MenuEntry[] {
  return Array.from({ length: n }, (_, i) => ({
    id: `m${i}`,
    label: `Item ${i}`,
    onSelect: () => pick(`m${i}`),
  }))
}

/** 菜单的几何：每项 28 px、上下内边距 0（jsdom 不加载样式）、可视高度 100 px；scrollTop 记在元素上 */
function stubLayout(row = 28, visible = 100) {
  const tops = new WeakMap<Element, number>()
  const isItem = (el: Element) => el.matches('[role^="menuitem"]')
  const menuOf = (el: Element) => el.closest<HTMLElement>('[role="menu"]')
  const index = (el: Element) => {
    const page = el.closest('[data-page]') ?? menuOf(el)
    return page ? [...page.querySelectorAll('[role^="menuitem"]')].indexOf(el) : -1
  }
  vi.spyOn(HTMLElement.prototype, 'offsetTop', 'get').mockImplementation(function (
    this: HTMLElement,
  ) {
    return isItem(this) ? row * index(this) : 0
  })
  vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockImplementation(function (
    this: HTMLElement,
  ) {
    return isItem(this) ? row : this.matches('[role="menu"]') ? visible : 0
  })
  vi.spyOn(HTMLElement.prototype, 'offsetParent', 'get').mockImplementation(function (
    this: HTMLElement,
  ) {
    return isItem(this) ? menuOf(this) : null
  })
  vi.spyOn(Element.prototype, 'clientHeight', 'get').mockImplementation(function (this: Element) {
    return this.matches('[role="menu"]') ? visible : 0
  })
  vi.spyOn(Element.prototype, 'scrollHeight', 'get').mockImplementation(function (this: Element) {
    return this.matches('[role="menu"]')
      ? row * this.querySelectorAll('[role^="menuitem"]').length
      : 0
  })
  vi.spyOn(Element.prototype, 'scrollTop', 'get').mockImplementation(function (this: Element) {
    return tops.get(this) ?? 0
  })
  vi.spyOn(Element.prototype, 'scrollTop', 'set').mockImplementation(function (
    this: Element,
    v: number,
  ) {
    tops.set(this, v)
  })
}

function dropdown(list: MenuEntry[], extra: React.ReactNode = null) {
  wrap(
    <>
      <DropdownMenu
        items={list}
        label="菜单"
        renderTrigger={(p) => (
          <button type="button" {...p}>
            打开
          </button>
        )}
      />
      {extra}
    </>,
  )
  return screen.getByRole('button', { name: '打开' })
}

/** 一次完整的指针点击（按下 → 松开 → click），坐标相同 */
function press(el: Element, x = 0, y = 0, button = 0) {
  const init = { clientX: x, clientY: y, button, pointerType: 'mouse' }
  fireEvent.pointerDown(el, init)
  fireEvent.mouseDown(el, init)
  fireEvent.pointerUp(el, init)
  fireEvent.mouseUp(el, init)
  fireEvent.click(el, init)
}

describe('useFloating', () => {
  it('浮层自己的滚动不重新定位；页面上其他容器的滚动照常跟随锚点', async () => {
    const trigger = dropdown(items(12))
    fireEvent.click(trigger)
    const menu = await screen.findByRole('menu')
    await frame()
    const rect = vi.spyOn(trigger, 'getBoundingClientRect')
    fireEvent.scroll(menu)
    await frame()
    expect(rect).not.toHaveBeenCalled()
    fireEvent.scroll(document)
    await frame()
    expect(rect).toHaveBeenCalled()
  })

  it('测量自然尺寸不改样式：重新定位时不会把 max-height 写成 none（那一刻菜单不再溢出，滚动位置被夹回 0）', async () => {
    const trigger = dropdown(items(12))
    fireEvent.click(trigger)
    const menu = await screen.findByRole('menu')
    await frame()
    const olds: string[] = []
    const collect = (list: MutationRecord[]) => olds.push(...list.map((r) => r.oldValue ?? ''))
    const mo = new MutationObserver(collect)
    mo.observe(menu, { attributes: true, attributeFilter: ['style'], attributeOldValue: true })
    fireEvent(window, new Event('resize'))
    await frame()
    collect(mo.takeRecords())
    mo.disconnect()
    expect(olds.some((v) => /max-height:\s*none/.test(v))).toBe(false)
  })
})

describe('Menu 的滚入视野', () => {
  it('悬停改变高亮不滚动菜单；键盘导航把高亮项滚进视野，只滚菜单自己', async () => {
    stubLayout()
    const into = vi.spyOn(Element.prototype, 'scrollIntoView')
    const trigger = dropdown(items(12))
    fireEvent.click(trigger)
    const menu = await screen.findByRole('menu')
    // 悬停在底部看不见的一项上（真实移动）：高亮它，但不滚动
    const far = screen.getByText('Item 8')
    fireEvent.pointerMove(far, { clientX: 20, clientY: 90 })
    fireEvent.pointerMove(far, { clientX: 21, clientY: 92 })
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-8$/)
    expect(menu.scrollTop).toBe(0)
    // 键盘：End 把最后一项滚到底边（11 × 28 + 28 − 100）
    fireEvent.keyDown(menu, { key: 'End' })
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-11$/)
    expect(menu.scrollTop).toBe(236)
    // Home 回到顶部
    fireEvent.keyDown(menu, { key: 'Home' })
    expect(menu.scrollTop).toBe(0)
    expect(into).not.toHaveBeenCalled()
  })
})

describe('PagedMenu：换页之后重新预留', () => {
  it('进入更高的子页：卡片的预留随之增大（底边 = 锚点 + 间距 + 新页高度 + 边距）', () => {
    vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.matches('.ui-pmenu-page')
        ? 28 * this.querySelectorAll('[role^="menuitem"]').length
        : 0
    })
    const reserve = vi.fn<(id: symbol, bottom: number | null) => void>()
    const boundary: FloatingBoundary = {
      container: null,
      rect: () => new DOMRect(0, 0, 400, 600),
      reserve,
    }
    function Tall() {
      const [key, setKey] = React.useState<'root' | 'tall'>('root')
      const root: MenuPage = {
        key: 'root',
        label: '操作',
        'data-testid': 'pm',
        items: [{ id: 'more', label: 'More', submenu: true, onSelect: () => setKey('tall') }],
      }
      const tall: MenuPage = {
        key: 'tall',
        label: 'Tall',
        'data-testid': 'pm',
        back: { label: 'Tall', ariaLabel: '返回', onSelect: () => setKey('root') },
        items: items(10),
      }
      return (
        <PagedMenu
          open
          onClose={() => {}}
          anchor={{ left: 10, top: 10, width: 0, height: 0 }}
          page={key === 'root' ? root : tall}
          direction={1}
        />
      )
    }
    wrap(
      <FloatingContext value={boundary}>
        <Tall />
      </FloatingContext>,
    )
    const first = reserve.mock.calls.at(-1)?.[1] ?? 0
    press(screen.getByTestId('pm-item-more'))
    // 返回行 + 10 项 = 11 × 28；锚点底边 10 + 间距 4 + 边距 8
    expect(reserve.mock.calls.at(-1)?.[1]).toBe(10 + 4 + 11 * 28 + 8)
    expect(reserve.mock.calls.at(-1)?.[1]).toBeGreaterThan(first)
  })
})
