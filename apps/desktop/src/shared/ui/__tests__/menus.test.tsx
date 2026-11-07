/**
 * 菜单的滚动、悬停进入子页与点外关闭（jsdom 不排版：需要几何的用例把尺寸写进 getter）：
 * - useFloating：浮层自己的滚动不重新定位；测量自然尺寸时不改样式（不把 max-height 写成 none——那一刻菜单不再溢出，
 *   引擎会把滚动位置夹回 0）；
 * - Menu：悬停改变高亮不滚动菜单；键盘导航只滚菜单自己（不调用会连带滚动卡片的 scrollIntoView）；
 * - PagedMenu 悬停进入子页：200 ms 边界、竖直抖动重新计时 / 横向移动不重计、页面出现时压在指针下的「›」不自动进入、
 *   离开 / 按下 / 按键 / 触摸 / 禁用项都不进入；悬停进入的子页不预先高亮，原处点击（含 1–3 px 抖动）被吞掉、离开
 *   6 px 之后照常；键盘与点击进入不变；换页之后重新预留卡片高度；
 * - 点外面关闭菜单时那次点击被吞掉（Menu 与 PagedMenu），下一次点击照常。
 */
import { act, fireEvent, render, screen } from '@testing-library/react'
import * as React from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport, type Transport } from '../../ipc'
import { boot } from '../../ipc/boot'
import { MotionRoot } from '../../motion/MotionRoot'
import { ms, timing } from '../../motion/tokens'
import { DropdownMenu, PagedMenu, Popover, type MenuEntry, type MenuPage } from '..'
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

describe('useFloating：滚动容器在浮层更深处（排除应用选择器）', () => {
  it('浮层正被上一次的位置限高时，按传入的滚动容器与中间元素自己的上限量出真正的自然高度：翻到上方时整份放在锚点上方', () => {
    // 几何：浮层受上一次的 max-height 限在 226（重新打开时沿用上次的位置）；中间的选择器有自己的 360 px 上限、
    // 现在 202 高；候选列表可视 150、内容 400。真正的自然高度 = 226 + min(400 − 150, 360 − 202) = 384
    vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockImplementation(function (
      this: HTMLElement,
    ) {
      if (this.matches('.ui-popover')) return 226
      if (this.dataset.testid === 'mid') return 202
      return 0
    })
    vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(300)
    vi.spyOn(Element.prototype, 'clientHeight', 'get').mockImplementation(function (this: Element) {
      if (this.matches('.ui-popover')) return 226
      return this.getAttribute('data-testid') === 'list' ? 150 : 0
    })
    vi.spyOn(Element.prototype, 'scrollHeight', 'get').mockImplementation(function (this: Element) {
      if (this.matches('.ui-popover')) return 226
      return this.getAttribute('data-testid') === 'list' ? 400 : 0
    })
    const boundary: FloatingBoundary = { container: null, rect: () => new DOMRect(0, 0, 760, 480) }
    function Picker() {
      const anchor = React.useRef<HTMLButtonElement>(null)
      const list = React.useRef<HTMLDivElement>(null)
      return (
        <>
          <button
            type="button"
            ref={(el) => {
              anchor.current = el
              if (el)
                vi.spyOn(el, 'getBoundingClientRect').mockReturnValue(new DOMRect(40, 420, 80, 28))
            }}
          >
            添加
          </button>
          <Popover open onClose={() => {}} anchor={anchor} scroller={list} data-testid="pop">
            <div data-testid="mid" style={{ maxHeight: '360px' }}>
              <div ref={list} data-testid="list" />
            </div>
          </Popover>
        </>
      )
    }
    wrap(
      <FloatingContext value={boundary}>
        <Picker />
      </FloatingContext>,
    )
    const pop = screen.getByTestId('pop')
    // 下方 480 − 8 − (448 + 6) = 18 放不下，上方 420 − 6 − 8 = 406 放得下 384：翻到上方，底边贴在锚点上方 6 px
    expect(pop.style.top).toBe(`${420 - 6 - 384}px`)
    expect(pop.style.maxHeight).toBe('406px')
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

  it('重新打开时滚进视野的是这次的初始高亮项：上次用键盘停在靠下的项、关闭之后再打开，勾选项仍然可见', async () => {
    stubLayout()
    // 勾选第 2 项（顶边 28，底边 56：不滚动就在可视区里）
    const list = items(40).map((e, i) => ({ ...e, checked: i === 1 }))
    wrap(
      <DropdownMenu
        items={list}
        label="菜单"
        highlightSelected
        renderTrigger={(p) => (
          <button type="button" {...p}>
            打开
          </button>
        )}
      />,
    )
    const trigger = screen.getByRole('button', { name: '打开' })
    fireEvent.click(trigger)
    let menu = await screen.findByRole('menu')
    await frame()
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-1$/)
    fireEvent.keyDown(menu, { key: 'End' })
    expect(menu.scrollTop).toBe(40 * 28 - 100)
    // 关闭（再点一下触发器）、再打开：高亮回到勾选项，勾选项在可视区里
    fireEvent.click(trigger)
    expect(trigger.getAttribute('aria-expanded')).toBe('false')
    fireEvent.click(trigger)
    menu = await screen.findByRole('menu')
    await frame()
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-1$/)
    expect(menu.scrollTop).toBeLessThanOrEqual(28)
    expect(menu.scrollTop + 100).toBeGreaterThanOrEqual(56)
  })

  it('键盘打开（高亮第一项）同样：上次停在最后一项，再打开时第一项可见', async () => {
    stubLayout()
    const trigger = dropdown(items(40))
    fireEvent.keyDown(trigger, { key: 'ArrowDown' })
    let menu = await screen.findByRole('menu')
    await frame()
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-0$/)
    fireEvent.keyDown(menu, { key: 'End' })
    expect(menu.scrollTop).toBe(40 * 28 - 100)
    fireEvent.click(trigger)
    expect(trigger.getAttribute('aria-expanded')).toBe('false')
    fireEvent.keyDown(trigger, { key: 'ArrowDown' })
    menu = await screen.findByRole('menu')
    await frame()
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-0$/)
    expect(menu.scrollTop).toBe(0)
  })

  it('键盘导航到已经高亮、却被滚出视野的项（先滚轮滚开再按 End）：同样滚回来', async () => {
    stubLayout()
    const trigger = dropdown(items(40))
    fireEvent.click(trigger)
    const menu = await screen.findByRole('menu')
    await frame()
    fireEvent.keyDown(menu, { key: 'End' })
    expect(menu.scrollTop).toBe(40 * 28 - 100)
    menu.scrollTop = 0
    fireEvent.keyDown(menu, { key: 'End' })
    expect(menu.scrollTop).toBe(40 * 28 - 100)
  })
})

describe('PagedMenu 的滚入视野', () => {
  it('重新打开时滚进视野的是这次的初始高亮项（不是上次留下的高亮）', async () => {
    stubLayout()
    function Reopen() {
      const [open, setOpen] = React.useState(true)
      return (
        <>
          <button type="button" onClick={() => setOpen((o) => !o)}>
            开关
          </button>
          <PagedMenu
            open={open}
            onClose={() => setOpen(false)}
            anchor={{ left: 10, top: 10, width: 0, height: 0 }}
            page={{ key: 'root', label: '操作', 'data-testid': 'pm', items: items(40) }}
            direction={1}
            initialActive="first"
          />
        </>
      )
    }
    wrap(<Reopen />)
    let menu = screen.getByRole('menu')
    await frame()
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-root-0$/)
    fireEvent.keyDown(menu, { key: 'End' })
    expect(menu.scrollTop).toBe(40 * 28 - 100)
    const toggle = screen.getByRole('button', { name: '开关' })
    // 菜单开着时点外面只关菜单（这一下被吞掉）：用 click 直接切换
    fireEvent.click(toggle)
    fireEvent.click(toggle)
    menu = await screen.findByRole('menu')
    await frame()
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-root-0$/)
    expect(menu.scrollTop).toBe(0)
  })
})

// ---------------------------------------------------------------------------
// PagedMenu：悬停进入子页

/** 两页的分页菜单：根页「Alpha · More › · Beta · ── · Off ›（禁用）」，子页「‹ More · Xray · Yank」 */
function Paged({ onPick, onClose }: { onPick: (id: string) => void; onClose: () => void }) {
  const [key, setKey] = React.useState<'root' | 'sub'>('root')
  const [dir, setDir] = React.useState<1 | -1>(1)
  const enter = () => {
    setDir(1)
    setKey('sub')
  }
  const root: MenuPage = {
    key: 'root',
    label: '操作',
    'data-testid': 'pm',
    items: [
      { id: 'a', label: 'Alpha', onSelect: () => onPick('a') },
      { id: 'more', label: 'More', submenu: true, onSelect: enter },
      { id: 'b', label: 'Beta', onSelect: () => onPick('b') },
      { type: 'separator', id: 's' },
      { id: 'off', label: 'Off', submenu: true, disabled: true, onSelect: enter },
    ],
  }
  const sub: MenuPage = {
    key: 'sub',
    label: 'More',
    'data-testid': 'pm',
    back: {
      label: 'More',
      ariaLabel: '返回',
      onSelect: () => {
        setDir(-1)
        setKey('root')
      },
    },
    items: [
      { id: 'x', label: 'Xray', onSelect: () => onPick('x') },
      { id: 'y', label: 'Yank', onSelect: () => onPick('y') },
    ],
  }
  return (
    <PagedMenu
      open
      onClose={onClose}
      anchor={{ left: 10, top: 10, width: 0, height: 0 }}
      page={key === 'root' ? root : sub}
      direction={dir}
      initialActive="none"
    />
  )
}

const DWELL = ms(timing.submenuDwell)
const currentPage = () =>
  document.querySelector('[role="menu"] [data-page]:not([aria-hidden])')?.getAttribute('data-page')
const item = (id: string) =>
  document.querySelector<HTMLElement>(
    `[role="menu"] [data-page]:not([aria-hidden]) [data-testid="pm-item-${id}"]`,
  ) as HTMLElement
function move(el: Element, x: number, y: number, pointerType = 'mouse') {
  fireEvent.pointerMove(el, { clientX: x, clientY: y, pointerType })
}
const wait = (t: number) => act(() => vi.advanceTimersByTime(t))

function mountPaged() {
  vi.useFakeTimers()
  const onPick = vi.fn<(id: string) => void>()
  const onClose = vi.fn<() => void>()
  wrap(<Paged onPick={onPick} onClose={onClose} />)
  return { onPick, onClose, menu: screen.getByRole('menu') }
}

/** 指针从 Alpha 移进「More ›」：（基线）→ Alpha（真实移动）→ More */
function hoverMore(y = 40) {
  move(item('a'), 20, 12)
  move(item('a'), 20, 14)
  move(item('more'), 20, y)
}

describe('PagedMenu：悬停进入子页', () => {
  it('从别的行移进「›」项、停住 200 ms 进入子页（199 ms 时还没有）', () => {
    mountPaged()
    hoverMore()
    wait(DWELL - 1)
    expect(currentPage()).toBe('root')
    wait(1)
    expect(currentPage()).toBe('sub')
  })

  it('停住只看竖直位移：横向移向「›」不重新计时，竖直移动超过 6 px 重新计时', () => {
    mountPaged()
    hoverMore(40)
    wait(150)
    move(item('more'), 80, 41)
    wait(DWELL - 150)
    expect(currentPage()).toBe('sub')
  })

  it('竖直移动超过 6 px：从新位置重新计时', () => {
    mountPaged()
    hoverMore(40)
    wait(150)
    move(item('more'), 20, 47)
    wait(DWELL - 1)
    expect(currentPage()).toBe('root')
    wait(1)
    expect(currentPage()).toBe('sub')
  })

  it('页面出现时就压在指针下的「›」不会自动进入：要离开再进入才计时（返回根页之后同样）', () => {
    const { onPick } = mountPaged()
    // 第一次真实移动就落在 More 上：它是页面出现时压在指针下的那一行
    move(item('more'), 20, 40)
    move(item('more'), 21, 40)
    wait(DWELL * 3)
    expect(currentPage()).toBe('root')
    // 离开再进入：计时
    move(item('a'), 21, 14)
    move(item('more'), 21, 40)
    wait(DWELL)
    expect(currentPage()).toBe('sub')
    // 离开换页处、点返回行回到根页：指针又停在 More 上，不会马上又翻进去
    move(item('x'), 60, 40)
    press(item('back'), 60, 12)
    expect(onPick).not.toHaveBeenCalled()
    expect(currentPage()).toBe('root')
    move(item('more'), 60, 40)
    move(item('more'), 61, 41)
    wait(DWELL * 3)
    expect(currentPage()).toBe('root')
  })

  it('打开后的第一次移动（只记基线）落在哪一行，就当作出现时压在指针下的那一行：紧接着第一次真实移动就落到「›」上也算移进来', () => {
    mountPaged()
    move(item('a'), 20, 12)
    move(item('more'), 20, 40)
    wait(DWELL)
    expect(currentPage()).toBe('sub')
  })

  it('换页时按指针最近一次的位置认定出现时压在它下面的那一行：返回根页后第一次移动就直接落到「›」上也算移进来', () => {
    const { menu } = mountPaged()
    hoverMore(40)
    wait(DWELL)
    expect(currentPage()).toBe('sub')
    // 离开换页处（解除防误触），停在子页顶部的返回行上
    move(item('x'), 60, 40)
    move(item('back'), 60, 12)
    // 命中测试（jsdom 不排版）：上面一行是根页的 Alpha，往下是 More
    const hit = (_x: number, y: number) => [y < 26 ? item('a') : item('more')]
    Object.defineProperty(document, 'elementsFromPoint', { configurable: true, value: hit })
    try {
      fireEvent.keyDown(menu, { key: 'ArrowLeft' })
      expect(currentPage()).toBe('root')
      // 出现时指针压着 Alpha；第一次移动（快速移动合并成的一次事件）直接落到 More 上：计时
      move(item('more'), 60, 40)
      wait(DWELL)
      expect(currentPage()).toBe('sub')
      // 反过来：出现时指针就压着「›」（换页前停在返回行下面一行），之后的抖动不计时
      move(item('x'), 60, 44)
      fireEvent.keyDown(menu, { key: 'ArrowLeft' })
      expect(currentPage()).toBe('root')
      move(item('more'), 61, 45)
      wait(DWELL * 3)
      expect(currentPage()).toBe('root')
    } finally {
      delete (document as { elementsFromPoint?: unknown }).elementsFromPoint
    }
  })

  it('离开菜单、按下、按键、触摸、禁用的「›」：不进入', () => {
    const { menu } = mountPaged()
    hoverMore()
    move(document.body, 300, 300)
    wait(DWELL * 2)
    expect(currentPage()).toBe('root')
    move(item('a'), 20, 14)
    move(item('more'), 20, 40)
    fireEvent.pointerDown(item('more'), { clientX: 20, clientY: 40, pointerType: 'mouse' })
    wait(DWELL * 2)
    expect(currentPage()).toBe('root')
    move(item('a'), 20, 14)
    move(item('more'), 20, 40)
    fireEvent.keyDown(menu, { key: 'Shift' })
    wait(DWELL * 2)
    expect(currentPage()).toBe('root')
    move(item('a'), 20, 14, 'touch')
    move(item('more'), 20, 40, 'touch')
    wait(DWELL * 2)
    expect(currentPage()).toBe('root')
    move(item('b'), 20, 60)
    move(item('off'), 20, 100)
    wait(DWELL * 2)
    expect(currentPage()).toBe('root')
  })

  it('悬停进入的子页不预先高亮；原处点击（含 1–3 px 抖动、点到菜单外）被吞掉，离开 6 px 之后照常选择', () => {
    const { onPick, onClose, menu } = mountPaged()
    hoverMore(40)
    wait(DWELL)
    expect(currentPage()).toBe('sub')
    expect(menu.hasAttribute('aria-activedescendant')).toBe(false)
    press(item('x'), 20, 40)
    move(item('x'), 22, 41)
    press(item('x'), 22, 41)
    move(item('x'), 23, 43)
    press(item('y'), 23, 43)
    // 新页变窄、指针落到了菜单外：同样只吞掉，不关菜单
    press(document.body, 23, 43)
    expect(onPick).not.toHaveBeenCalled()
    expect(onClose).not.toHaveBeenCalled()
    expect(currentPage()).toBe('sub')
    move(item('x'), 30, 40)
    press(item('x'), 30, 40)
    expect(onPick).toHaveBeenCalledWith('x')
    expect(onClose).toHaveBeenCalledTimes(1)
  })

  it('悬停换页之后又换了页（← 返回、内容变化换回根页）：指针还没离开换页处，点击照样被吞掉', () => {
    const { onPick, onClose, menu } = mountPaged()
    hoverMore(40)
    wait(DWELL)
    expect(currentPage()).toBe('sub')
    fireEvent.keyDown(menu, { key: 'ArrowLeft' })
    expect(currentPage()).toBe('root')
    // 指针停在根页的 More 上：不选择（不进入子页）、不关菜单
    press(item('more'), 21, 41)
    expect(currentPage()).toBe('root')
    expect(onPick).not.toHaveBeenCalled()
    expect(onClose).not.toHaveBeenCalled()
  })

  it('菜单开着时，菜单外的指针移动不交给下面的视图；菜单里的悬停照常', () => {
    vi.useFakeTimers()
    const under = vi.fn<() => void>()
    wrap(
      <>
        <Paged onPick={() => {}} onClose={() => {}} />
        {/* oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 用例里的悬停目标 */}
        <div data-testid="under" onPointerMove={under}>
          下面的视图
        </div>
      </>,
    )
    const menu = screen.getByRole('menu')
    move(screen.getByTestId('under'), 300, 300)
    move(screen.getByTestId('under'), 302, 301)
    expect(under).not.toHaveBeenCalled()
    move(item('b'), 20, 60)
    move(item('b'), 21, 61)
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-root-2$/)
  })

  it('键盘（→）与点击进入的子页：照旧高亮第一项，立即点击照常选择（不加保护）', () => {
    const { onPick, menu } = mountPaged()
    fireEvent.keyDown(menu, { key: 'ArrowDown' })
    fireEvent.keyDown(menu, { key: 'ArrowDown' })
    fireEvent.keyDown(menu, { key: 'ArrowRight' })
    expect(currentPage()).toBe('sub')
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-sub--1$/)
    fireEvent.keyDown(menu, { key: 'ArrowLeft' })
    expect(currentPage()).toBe('root')
    press(item('more'), 20, 40)
    expect(currentPage()).toBe('sub')
    expect(menu.getAttribute('aria-activedescendant')).toMatch(/-sub--1$/)
    press(item('x'), 20, 40)
    expect(onPick).toHaveBeenCalledWith('x')
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

describe('点外面关闭菜单：只关菜单', () => {
  it('Menu：点外面的按钮只关菜单，按钮收不到这次按下与点击；下一次点击照常', async () => {
    const outside = vi.fn<() => void>()
    const down = vi.fn<() => void>()
    const trigger = dropdown(
      items(3),
      <button type="button" onClick={outside} onPointerDown={down}>
        外面
      </button>,
    )
    fireEvent.click(trigger)
    await screen.findByRole('menu')
    const btn = screen.getByRole('button', { name: '外面' })
    press(btn, 300, 300)
    expect(trigger.getAttribute('aria-expanded')).toBe('false')
    expect(outside).not.toHaveBeenCalled()
    expect(down).not.toHaveBeenCalled()
    press(btn, 300, 300)
    expect(outside).toHaveBeenCalledTimes(1)
  })

  it('Menu：右键点外面照常（关闭菜单，右键菜单事件照样送达）', async () => {
    const ctx = vi.fn<() => void>()
    const trigger = dropdown(
      items(3),
      // oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 用例里的右键目标
      <div data-testid="area" onContextMenu={ctx}>
        区域
      </div>,
    )
    fireEvent.click(trigger)
    await screen.findByRole('menu')
    const area = screen.getByTestId('area')
    fireEvent.pointerDown(area, { button: 2, pointerType: 'mouse' })
    fireEvent.contextMenu(area, { button: 2 })
    expect(trigger.getAttribute('aria-expanded')).toBe('false')
    expect(ctx).toHaveBeenCalledTimes(1)
  })

  it('macOS 上 Ctrl + 主键点外面是右键：关闭菜单，右键菜单事件照样送达（与真正的右键一致）', async () => {
    const os = boot.os
    boot.os = 'macos'
    try {
      const ctx = vi.fn<() => void>()
      const down = vi.fn<() => void>()
      const trigger = dropdown(
        items(3),
        // oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 用例里的右键目标
        <div data-testid="area" onContextMenu={ctx} onPointerDown={down}>
          区域
        </div>,
      )
      fireEvent.click(trigger)
      await screen.findByRole('menu')
      const area = screen.getByTestId('area')
      const init = { button: 0, ctrlKey: true, pointerType: 'mouse' }
      fireEvent.pointerDown(area, init)
      fireEvent.mouseDown(area, init)
      fireEvent.contextMenu(area, init)
      expect(trigger.getAttribute('aria-expanded')).toBe('false')
      expect(down).toHaveBeenCalledTimes(1)
      expect(ctx).toHaveBeenCalledTimes(1)
    } finally {
      boot.os = os
    }
  })

  it('Windows / Linux 上 Ctrl + 点外面仍是主键点击（剪贴板里是多选）：只关菜单', async () => {
    const os = boot.os
    boot.os = 'windows'
    try {
      const outside = vi.fn<() => void>()
      const trigger = dropdown(
        items(3),
        <button type="button" onClick={outside}>
          外面
        </button>,
      )
      fireEvent.click(trigger)
      await screen.findByRole('menu')
      const btn = screen.getByRole('button', { name: '外面' })
      const init = { button: 0, ctrlKey: true, pointerType: 'mouse' }
      fireEvent.pointerDown(btn, init)
      fireEvent.mouseDown(btn, init)
      fireEvent.pointerUp(btn, init)
      fireEvent.mouseUp(btn, init)
      fireEvent.click(btn, init)
      expect(trigger.getAttribute('aria-expanded')).toBe('false')
      expect(outside).not.toHaveBeenCalled()
    } finally {
      boot.os = os
    }
  })

  it('PagedMenu：macOS 上 Ctrl + 主键点外面同样是右键，右键菜单事件照样送达', () => {
    const os = boot.os
    boot.os = 'macos'
    try {
      const ctx = vi.fn<() => void>()
      const onClose = vi.fn<() => void>()
      wrap(
        <>
          <PagedMenu
            open
            onClose={onClose}
            anchor={{ left: 10, top: 10, width: 0, height: 0 }}
            page={{ key: 'root', label: '操作', items: items(3) }}
            direction={1}
          />
          {/* oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 用例里的右键目标 */}
          <div data-testid="area" onContextMenu={ctx}>
            区域
          </div>
        </>,
      )
      const area = screen.getByTestId('area')
      const init = { button: 0, ctrlKey: true, pointerType: 'mouse' }
      fireEvent.pointerDown(area, init)
      fireEvent.mouseDown(area, init)
      fireEvent.contextMenu(area, init)
      expect(onClose).toHaveBeenCalledTimes(1)
      expect(ctx).toHaveBeenCalledTimes(1)
    } finally {
      boot.os = os
    }
  })

  it('PagedMenu：点外面的按钮只关菜单，按钮收不到这次点击', () => {
    const outside = vi.fn<() => void>()
    const onClose = vi.fn<() => void>()
    wrap(
      <>
        <PagedMenu
          open
          onClose={onClose}
          anchor={{ left: 10, top: 10, width: 0, height: 0 }}
          page={{ key: 'root', label: '操作', items: items(3) }}
          direction={1}
        />
        <button type="button" onClick={outside}>
          外面
        </button>
      </>,
    )
    press(screen.getByRole('button', { name: '外面' }), 300, 300)
    expect(onClose).toHaveBeenCalledTimes(1)
    expect(outside).not.toHaveBeenCalled()
  })
})
