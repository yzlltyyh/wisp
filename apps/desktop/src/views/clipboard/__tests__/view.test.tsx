/**
 * 剪贴板视图：
 * - 打开不查库：挂载时不发任何命令；「置顶 / 最近」两组；
 * - 选中指示器在 DOM 中只有一个，混合行高下 y / height 与行严格对齐，键盘移动跳过分组标题；
 *   按键重复与换列表来源时直接到位；实时变化时与行的位移同一条曲线；
 * - rowIn 打开动效期间按 ↓ 立即响应；预取之后的第一次 Space 在同一次提交里打开预览；
 * - 实时插入到「最近」顶部、选中项不跳；已有行 FLIP 平移；updated 原地替换；removed 后落到相邻条目；
 * - 输入即搜索（去抖、过期结果作废、清空后回到最近列表且不发命令）、高亮；会话内高度只增不减；
 * - 暂停横幅 / 空状态 / 无结果；分页；右键菜单；
 * - 悬停 300 ms 的预览卡片，原图只在悬停 / 预览时请求；
 * - Wayland 下由 WebView 写剪贴板并切到 toast 视图；卸载时退订、释放。
 */
import { act, fireEvent, screen, waitFor, within, type RenderResult } from '@testing-library/react'
import type { ReactElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { lastPrewarm } from '../../../shared/perf/prewarm'
import { setTransport } from '../../../shared/ipc'
import { t } from '../../../shared/i18n/i18n'
import { relativeTime } from '../../../shared/i18n/relativeTime'
import { MotionRoot } from '../../../shared/motion/MotionRoot'
import { ClipboardView, prewarmClipboardRender } from '../ClipboardView'
import { rowLabel } from '../ClipRow'
import { preloadPreview } from '../previewModule'
import { resetWebviewClipboardForTest, statusHoldMs } from '../useClipboard'
import { GROUP_HEIGHT, IMAGE_ROW_HEIGHT, LIST_PADDING, OPEN_MIN_HEIGHT, ROW_HEIGHT } from '../model'
import {
  armPointer,
  fakeIpc,
  hit,
  idleTransport,
  key,
  meta,
  NOW,
  payload,
  renderIn,
  restorePlatform,
  sample,
  setPlatform,
  testRunner,
  viewProps,
  type FakeIpc,
} from './helpers'

let ipc: FakeIpc

function mount(items = sample(), extra = {}, props = {}) {
  const p = viewProps<'clipboard'>(payload(items, extra), props)
  const r = renderIn(<ClipboardView {...p} />)
  return { props: p, ...r }
}

/** 重新渲染（同一棵树：外面同样包着 MotionRoot，否则会整棵重新挂载） */
const rerenderIn = (r: RenderResult, ui: ReactElement) => r.rerender(<MotionRoot>{ui}</MotionRoot>)

const search = () => screen.getByTestId('clip-search') as HTMLInputElement
const selected = () => search().getAttribute('aria-activedescendant')
const section = () => screen.getByTestId('view-clipboard')
const rowIds = () =>
  screen.getAllByTestId('clip-row').map((r) => Number(r.getAttribute('data-clip-id')))
/** 行在滚动内容层里的 y（VirtualList 的 translateY） */
const rowY = (id: number) => {
  const row = document.getElementById(`clip-row-${id}`)?.closest<HTMLElement>('.ui-vlist-row')
  return Number(/translateY\(([\d.]+)px\)/.exec(row?.style.transform ?? '')?.[1])
}

beforeEach(() => {
  ipc = fakeIpc()
  ipc.results.clipboard_paste = { type: 'pasted' }
  setPlatform('macos')
})

afterEach(() => {
  restorePlatform()
  setTransport(idleTransport())
  resetWebviewClipboardForTest()
  vi.useRealTimers()
})

/** 指示器（本视图自绘，挂在 VirtualList 内容层里） */
const indicator = () => document.querySelector<HTMLElement>('[data-testid="clip-indicator"]')
/** 等一帧（Motion 在帧循环里把 MotionValue 写到 DOM） */
const nextFrame = () =>
  act(
    () =>
      new Promise<void>((resolve) => {
        requestAnimationFrame(() => requestAnimationFrame(() => resolve()))
      }),
  )
/** 等一帧（单个 rAF） */
const oneFrame = () =>
  act(
    () =>
      new Promise<void>((resolve) => {
        requestAnimationFrame(() => resolve())
      }),
  )
const indicatorY = () =>
  Number(/translateY\((-?[\d.]+)px\)/.exec(indicator()?.style.transform ?? '')?.[1] ?? 0)

describe('打开', () => {
  it('首屏只用载荷里的缓存（不发任何命令）；搜索框自动聚焦；默认选中第一条', () => {
    mount()
    expect(ipc.calls).toEqual([])
    expect(document.activeElement).toBe(search())
    expect(selected()).toBe('clip-row-1')
    expect(rowIds()).toEqual([1, 2, 3, 4, 5, 6, 7, 8])
    // 分组标题：置顶 / 最近
    const groups = [...document.querySelectorAll('.clip-group')].map((g) => g.textContent)
    expect(groups).toEqual([t('clipboard.group.pinned'), t('clipboard.group.recent')])
  })

  it('唤出后焦点不在视图里时（挂载时窗口不可见），获得焦点时补聚焦搜索框', () => {
    const p = viewProps<'clipboard'>(payload(sample()), { focused: false })
    const r = renderIn(<ClipboardView {...p} />)
    act(() => search().blur())
    expect(document.activeElement).not.toBe(search())
    rerenderIn(r, <ClipboardView {...p} focused />)
    expect(document.activeElement).toBe(search())
  })

  it('focused 一开始就是 true、挂载时窗口还不可见（autoFocus 没生效）：面板进入 entering / shown 时补聚焦', () => {
    const p = viewProps<'clipboard'>(payload(sample()), { phase: 'preparing' })
    const r = renderIn(<ClipboardView {...p} />)
    act(() => search().blur())
    expect(document.activeElement).not.toBe(search())
    // prepare 阶段不补（窗口还没显示，也不依赖 rAF）
    rerenderIn(r, <ClipboardView {...p} phase="preparing" />)
    expect(document.activeElement).not.toBe(search())
    rerenderIn(r, <ClipboardView {...p} phase="entering" />)
    expect(document.activeElement).toBe(search())
    // 显示期间窗口重新获得焦点：焦点丢了就补
    act(() => search().blur())
    act(() => {
      window.dispatchEvent(new FocusEvent('focus'))
    })
    expect(document.activeElement).toBe(search())
  })

  it('点在非输入区域（列表空白、行、按钮）时焦点不离开搜索框；全文预览的正文可以选中文字', async () => {
    ipc.results.clipboard_get = () => null
    await preloadPreview()
    mount(sample(), { paused: true })
    const prevented = (el: Element) => !fireEvent.mouseDown(el, { button: 0 })
    expect(prevented(document.querySelector('.clip-listwrap') as Element)).toBe(true)
    // 可以拖出的行：mousedown 照常（否则拖不出去），松开时焦点回到搜索框
    const row = document.getElementById('clip-row-3') as Element
    expect(row.getAttribute('draggable')).toBe('true')
    expect(prevented(row)).toBe(false)
    act(() => search().blur())
    fireEvent.pointerUp(row)
    expect(document.activeElement).toBe(search())
    expect(prevented(within(screen.getByTestId('clip-paused')).getByRole('button'))).toBe(true)
    expect(prevented(search())).toBe(false)
    key('Space')
    expect(prevented(screen.getByTestId('clip-preview-text'))).toBe(false)
    expect(prevented(screen.getByTestId('clip-preview-close'))).toBe(true)
  })

  it('焦点丢了（点过正文、按钮卸载之后）：可打印字符 / ⌫ 把焦点送回搜索框；带 ⌘ 的组合不管', () => {
    mount()
    act(() => search().blur())
    key('KeyA', { meta: true, target: document.body })
    expect(document.activeElement).not.toBe(search())
    key('KeyA', { key: 'a', target: document.body })
    expect(document.activeElement).toBe(search())
    act(() => search().blur())
    fireEvent.change(search(), { target: { value: 'ab' } })
    act(() => search().blur())
    key('Backspace', { target: document.body })
    expect(document.activeElement).toBe(search())
  })

  it('每个菜单第一次打开时焦点就进入菜单（浮层定位之前 focus() 不生效时补一次），↓ 立即移动高亮', async () => {
    // 模拟真实浏览器：visibility: hidden 的元素拿不到焦点（组件库的 Menu 在第一次定位之前就调用了 focus()）
    const original = HTMLElement.prototype.focus
    const spy = vi.spyOn(HTMLElement.prototype, 'focus').mockImplementation(function (
      this: HTMLElement,
      options?: FocusOptions,
    ) {
      if (this.getAttribute('role') === 'menu' && this.style.visibility === 'hidden') return
      original.call(this, options)
    })
    try {
      mount()
      key('KeyF', { meta: true })
      await waitFor(() =>
        expect(document.activeElement?.getAttribute('data-testid')).toBe('clip-filter-menu'),
      )
      fireEvent.keyDown(document.activeElement as Element, { key: 'ArrowDown' })
      expect(
        screen.getByTestId('clip-filter-menu-item-filter-favorite').hasAttribute('data-active'),
      ).toBe(true)
      key('Escape')
      await waitFor(() => expect(document.activeElement).toBe(search()))
      key('KeyK', { meta: true })
      await waitFor(() =>
        expect(document.activeElement?.getAttribute('data-testid')).toBe('clip-context-menu'),
      )
    } finally {
      spy.mockRestore()
    }
  })

  it('高度：clamp(内容, 下限, min(560, 上限))，显式 px', () => {
    mount([meta(1)])
    expect(section().style.height).toBe(`${OPEN_MIN_HEIGHT}px`)
  })
})

describe('选中指示器', () => {
  it('DOM 里只有一个；混合 44 / 64 行高下 y / height 与行严格对齐；键盘移动跳过分组标题', async () => {
    mount()
    const all = () => document.querySelectorAll<HTMLElement>('.ui-vlist-indicator')
    expect(all()).toHaveLength(1)
    // 画在内容层里、所有行的前面（行盖在它上面）
    expect(indicator()?.parentElement?.parentElement?.firstElementChild).toBe(
      indicator()?.parentElement,
    )
    // 1 → 2 → 3（跳过「最近」）→ 4 → 5（图片行，64）
    for (let i = 0; i < 4; i++) key('ArrowDown')
    expect(selected()).toBe('clip-row-5')
    const expected = LIST_PADDING + 2 * GROUP_HEIGHT + 4 * ROW_HEIGHT
    expect(rowY(5)).toBe(expected)
    await waitFor(() => {
      expect(indicatorY()).toBeCloseTo(expected, 0)
      expect(indicator()?.style.height).toBe(`${IMAGE_ROW_HEIGHT}px`)
    })
    key('ArrowDown')
    await waitFor(() => {
      expect(indicatorY()).toBeCloseTo(expected + IMAGE_ROW_HEIGHT, 0)
      expect(indicator()?.style.height).toBe(`${ROW_HEIGHT}px`)
    })
    expect(all()).toHaveLength(1)
  })

  it('按住 ↓ 的重复事件、按页 / 到首尾：指示器直接到位（不拖在选中项后面）', async () => {
    setPlatform('macos', false)
    mount()
    key('ArrowDown')
    for (let i = 0; i < 3; i++) key('ArrowDown', { repeat: true })
    expect(selected()).toBe('clip-row-5')
    await nextFrame()
    expect(indicatorY()).toBe(rowY(5))
    key('ArrowDown', { meta: true })
    await nextFrame()
    expect(indicatorY()).toBe(rowY(8))
  })

  it('单步移动：spring 追随，第一帧就有位移；连按时从当前位置连续地追（不瞬移），最终贴到选中行', async () => {
    setPlatform('macos', false)
    mount()
    await nextFrame()
    const y0 = indicatorY()
    key('ArrowDown')
    key('ArrowDown')
    key('ArrowDown')
    expect(selected()).toBe('clip-row-4')
    // 连按三次（不是重复事件，跨过一个分组标题）：从当前位置连续地往目标走，不往回走（逐帧位移的平滑程度要在真实
    // 浏览器里逐帧测量，jsdom 的帧间隔不稳定）；最终贴到选中行
    const ys = [y0]
    for (let i = 0; i < 12; i++) {
      // 逐帧采样，必须串行
      // oxlint-disable-next-line no-await-in-loop
      await oneFrame()
      ys.push(indicatorY())
    }
    const steps = ys.slice(1).map((y, i) => y - (ys[i] as number))
    expect(steps[0]).toBeGreaterThan(0)
    for (const d of steps) expect(d).toBeGreaterThan(-1)
    await waitFor(() => expect(indicatorY()).toBeCloseTo(rowY(4), 0))
  })

  it('换了列表来源（Tab 切过滤）：指示器直接到新的第一条，不在新内容上扫过', async () => {
    setPlatform('macos', false)
    ipc.results.clipboard_recent = ({ filter }: { filter: string }) => ({
      items: sample()
        .filter((m) => filter !== 'text' || m.kind === 'text')
        .map((item) => hit(item)),
      pinned_count: 1,
      next_offset: null,
      query: null,
      filter,
    })
    mount()
    for (let i = 0; i < 4; i++) key('ArrowDown', { repeat: i > 0 })
    key('Tab')
    const first = rowIds()[0] as number
    expect(selected()).toBe(`clip-row-${first}`)
    await nextFrame()
    expect(indicatorY()).toBe(rowY(first))
  })

  it('rowIn 打开动效期间按 ↓ 立即响应', () => {
    setPlatform('macos', false)
    mount()
    // 前 8 行的 stagger 还在播放（挂载后第一帧之前）
    key('ArrowDown')
    expect(selected()).toBe('clip-row-2')
    expect(document.getElementById('clip-row-2')?.getAttribute('aria-selected')).toBe('true')
  })
})

describe('实时插入与去重', () => {
  it('新条目插到「最近」顶部、选中项不跳；已有行从旧位置 FLIP 平移，新行延后淡入（rowInsert）', () => {
    setPlatform('macos', false)
    mount()
    key('ArrowDown')
    key('ArrowDown')
    expect(selected()).toBe('clip-row-3')
    const before = rowY(3)
    ipc.changed({ change: 'inserted', item: meta(20, { last_used_at: NOW, preview: 'new' }) })
    expect(rowIds().slice(0, 4)).toEqual([1, 2, 20, 3])
    expect(selected()).toBe('clip-row-3')
    // 行的定位已经是新位置，内容层先平移回旧位置（同一次提交里），再走到 0
    expect(rowY(3)).toBe(before + ROW_HEIGHT)
    const content = document.getElementById('clip-row-3')?.closest('.ui-vlist-row')
      ?.firstElementChild as HTMLElement
    expect(content.style.transform).toBe(`translateY(${-ROW_HEIGHT}px)`)
    // 分组标题也一起平移
    const header = document.querySelector('[data-group="recent"]')?.closest('.ui-vlist-row')
    expect(header?.firstElementChild?.getAttribute('style') ?? '').toBe('')
    // 新行：等位移让出空位再淡入（初始透明）
    const shell = document.getElementById('clip-row-20')?.parentElement as HTMLElement
    expect(shell.style.opacity).toBe('0')
  })

  it('置顶移到置顶区：跨过其他行的那一行抬起（压在上面、不透明底），其余行同一条曲线平移', () => {
    setPlatform('macos', false)
    ipc.results.clipboard_pin = ({ id, pinned }: { id: number; pinned: boolean }) =>
      meta(id, { ...sample().find((m) => m.id === id), pinned })
    mount()
    for (let i = 0; i < 5; i++) key('ArrowDown')
    expect(selected()).toBe('clip-row-6')
    key('KeyD', { meta: true })
    expect(rowIds().slice(0, 3)).toEqual([6, 1, 2])
    const row6 = document.getElementById('clip-row-6')?.closest<HTMLElement>('.ui-vlist-row')
    expect(row6?.hasAttribute('data-lift')).toBe(true)
    const row1 = document.getElementById('clip-row-1')?.closest<HTMLElement>('.ui-vlist-row')
    expect(row1?.hasAttribute('data-lift')).toBe(false)
    expect((row1?.firstElementChild as HTMLElement | null)?.style.transform).toBe(
      `translateY(${-ROW_HEIGHT}px)`,
    )
  })

  it('重复复制（touched）只更新时间并移到顶部，不新增行', () => {
    mount()
    ipc.changed({ change: 'touched', item: { ...sample()[7]!, last_used_at: NOW + 1 } })
    expect(rowIds()).toEqual([1, 2, 8, 3, 4, 5, 6, 7])
  })

  it('updated 按 id 原地替换（缩略图生成完成），顺序不变', () => {
    mount()
    const img = sample()[4]!
    ipc.changed({
      change: 'updated',
      item: { ...img, image: { ...img.image!, thumb_url: 'thumb/new' } },
    })
    expect(rowIds()).toEqual([1, 2, 3, 4, 5, 6, 7, 8])
    expect(document.querySelector('#clip-row-5 img')?.getAttribute('src')).toBe('wisp://thumb/new')
  })

  it('removed 后选中项落到相邻条目；cleared（保留置顶）只剩置顶项', () => {
    mount()
    key('ArrowDown')
    key('ArrowDown')
    ipc.changed({ change: 'removed', id: 3 })
    expect(selected()).toBe('clip-row-4')
    ipc.changed({ change: 'cleared', keep_pinned: true })
    expect(rowIds()).toEqual([1, 2])
  })

  it('长高时底部提示条在同一次提交里先停在卡片当前的底边（随后跟着卡片下移），不会被裁掉或先跳下去', () => {
    setPlatform('macos', false)
    mount([meta(1), meta(2)])
    const before = Number.parseFloat(section().style.height)
    ipc.changed({ change: 'inserted', item: meta(10, { last_used_at: NOW + 10 }) })
    ipc.changed({ change: 'inserted', item: meta(11, { last_used_at: NOW + 11 }) })
    ipc.changed({ change: 'inserted', item: meta(12, { last_used_at: NOW + 12 }) })
    ipc.changed({ change: 'inserted', item: meta(13, { last_used_at: NOW + 13 }) })
    const after = Number.parseFloat(section().style.height)
    expect(after).toBeGreaterThan(before)
    const hints = screen.getByTestId('clip-hints')
    const shift = Number(/translateY\((-?[\d.]+)px\)/.exec(hints.style.transform)?.[1])
    expect(shift).toBeLessThan(0)
    expect(-shift).toBeLessThanOrEqual(after - before)
  })

  it('会话内高度只增不减：插入让内容变高时长高', () => {
    mount([meta(1), meta(2)])
    const before = Number.parseFloat(section().style.height)
    for (let i = 10; i < 16; i++) {
      ipc.changed({ change: 'inserted', item: meta(i, { last_used_at: NOW + i }) })
    }
    expect(Number.parseFloat(section().style.height)).toBeGreaterThan(before)
  })
})

describe('输入即搜索', () => {
  beforeEach(() => {
    ipc.results.clipboard_search = ({ query, filter }: { query: string; filter: string }) => ({
      items: sample()
        .filter((m) => m.preview.includes(query))
        .map((item) =>
          hit(item, [
            { start: item.preview.indexOf(query), end: item.preview.indexOf(query) + query.length },
          ]),
        ),
      pinned_count: 0,
      next_offset: null,
      query,
      filter,
    })
  })

  it('去抖后检索，结果平铺（置顶命中在前）并高亮；清空后回到最近列表，不再发命令', async () => {
    mount()
    const height = section().style.height
    fireEvent.change(search(), { target: { value: 'l' } })
    fireEvent.change(search(), { target: { value: 'lt' } })
    expect(ipc.called('clipboard_search')).toHaveLength(0)
    await waitFor(() => expect(rowIds()).toEqual([7]))
    expect(ipc.called('clipboard_search')).toEqual([{ query: 'lt', filter: 'all', limit: 200 }])
    expect(document.querySelector('#clip-row-7 mark')?.textContent).toBe('lt')
    expect(selected()).toBe('clip-row-7')
    // 结果变少时卡片不缩小
    expect(section().style.height).toBe(height)
    fireEvent.change(search(), { target: { value: '' } })
    expect(rowIds()).toEqual([1, 2, 3, 4, 5, 6, 7, 8])
    expect(ipc.called('clipboard_search')).toHaveLength(1)
  })

  it('无结果：「没有匹配「q」的条目」；少于 3 个字时说明只搜了最近 2000 条、多输入几个字可以搜全部', async () => {
    mount()
    fireEvent.change(search(), { target: { value: 'zzz' } })
    const box = await screen.findByTestId('clip-no-result')
    expect(box).toHaveTextContent(t('clipboard.no_result.title', { query: 'zzz' }))
    expect(box).toHaveTextContent(t('clipboard.no_result.detail'))
    fireEvent.change(search(), { target: { value: '中文' } })
    await waitFor(() =>
      expect(screen.getByTestId('clip-no-result')).toHaveTextContent(
        t('clipboard.no_result.detail_short', { min: 3, limit: '2,000' }),
      ),
    )
  })

  it('检索期间又插入新条目时重新检索', async () => {
    mount()
    fireEvent.change(search(), { target: { value: 'eta' } })
    await waitFor(() => expect(ipc.called('clipboard_search')).toHaveLength(1))
    ipc.changed({ change: 'inserted', item: meta(30, { preview: 'zeta', last_used_at: NOW }) })
    await waitFor(() => expect(ipc.called('clipboard_search')).toHaveLength(2))
  })
})

describe('空状态、暂停态', () => {
  it('空状态文案与复制快捷键提示', () => {
    mount([])
    expect(screen.getByTestId('clip-empty')).toHaveTextContent(t('clipboard.empty.title'))
  })

  it('暂停横幅 + 恢复', async () => {
    mount(sample(), { paused: true })
    const banner = screen.getByTestId('clip-paused')
    expect(banner).toHaveTextContent(t('clipboard.paused.title'))
    fireEvent.click(within(banner).getByRole('button'))
    await waitFor(() => expect(ipc.called('clipboard_set_paused')).toEqual([{ paused: false }]))
    expect(screen.queryByTestId('clip-paused')).toBeNull()
  })

  it('没有提示条时卡片相应变矮', () => {
    const { unmount } = mount(sample(), { hint_bar: true })
    const withBar = Number.parseFloat(section().style.height)
    expect(screen.getByTestId('clip-hints')).toBeTruthy()
    unmount()
    mount(sample().slice(0, 2), { hint_bar: false })
    expect(screen.queryByTestId('clip-hints')).toBeNull()
    expect(Number.parseFloat(section().style.height)).toBeLessThanOrEqual(withBar)
  })
})

describe('分页', () => {
  it('有下一页时，选中项走到末尾附近就加载下一页并追加', async () => {
    const many = Array.from({ length: 30 }, (_, i) => meta(i + 1, { last_used_at: NOW - i * 1000 }))
    ipc.results.clipboard_recent = () => ({
      items: [hit(meta(100, { last_used_at: NOW - 100_000, preview: 'page 2' }))],
      pinned_count: 0,
      next_offset: null,
      query: null,
      filter: 'all',
    })
    const p = payload(many)
    p.page.next_offset = 30
    renderIn(<ClipboardView {...viewProps<'clipboard'>(p)} />)
    key('ArrowDown', { meta: true })
    await waitFor(() =>
      expect(ipc.called('clipboard_recent')).toEqual([{ offset: 30, limit: 200, filter: 'all' }]),
    )
    key('ArrowDown', { meta: true })
    await waitFor(() => expect(selected()).toBe('clip-row-100'))
  })
})

describe('鼠标', () => {
  it('单击粘贴；改为双击后单击只移动高亮', async () => {
    const { unmount } = mount()
    fireEvent.click(document.getElementById('clip-row-4') as HTMLElement)
    await waitFor(() => expect(ipc.called('clipboard_paste')).toEqual([{ id: 4, mode: 'normal' }]))
    unmount()
    mount(sample(), { paste_on_double_click: true })
    fireEvent.click(document.getElementById('clip-row-6') as HTMLElement)
    expect(selected()).toBe('clip-row-6')
    expect(ipc.called('clipboard_paste')).toHaveLength(1)
    fireEvent.doubleClick(document.getElementById('clip-row-6') as HTMLElement)
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(2))
  })

  it('右键菜单包含全部操作并标注快捷键；删除可执行', async () => {
    mount()
    fireEvent.contextMenu(document.getElementById('clip-row-3') as HTMLElement, {
      clientX: 100,
      clientY: 100,
    })
    const menu = await screen.findByTestId('clip-context-menu')
    const labels = within(menu)
      .getAllByRole('menuitem')
      .map((el) => el.textContent ?? '')
    expect(labels.some((l) => l.includes(t('clipboard.action.paste_plain')))).toBe(true)
    expect(labels.some((l) => l.includes(t('clipboard.action.copy_only')))).toBe(true)
    expect(labels.some((l) => l.includes(t('clipboard.action.favorite')))).toBe(true)
    expect(labels.some((l) => l.includes(t('clipboard.action.translate')))).toBe(true)
    fireEvent.click(
      within(menu)
        .getAllByRole('menuitem')
        .find((el) => el.textContent?.includes(t('clipboard.action.delete'))) as HTMLElement,
    )
    await waitFor(() => expect(ipc.called('clipboard_delete')).toEqual([{ id: 3, force: false }]))
  })

  it('送翻译：走 clipboard_send_to(translate)；命令未实现时退回 trigger_action(translate_selection, 正文, clipboard)', async () => {
    mount()
    const open = async (id: number) => {
      fireEvent.contextMenu(document.getElementById(`clip-row-${id}`) as HTMLElement, {
        clientX: 100,
        clientY: 100,
      })
      const menu = await screen.findByTestId('clip-context-menu')
      fireEvent.click(
        within(menu)
          .getAllByRole('menuitem')
          .find((el) => el.textContent?.includes(t('clipboard.action.translate'))) as HTMLElement,
      )
    }
    await open(3)
    await waitFor(() =>
      expect(ipc.called('clipboard_send_to')).toEqual([{ id: 3, target: 'translate' }]),
    )
    expect(ipc.called('trigger_action')).toHaveLength(0)
    ipc.results.clipboard_send_to = () => {
      throw { code: 'not_implemented', message: 'later', llm: null }
    }
    ipc.results.clipboard_get = () => ({
      meta: sample()[3],
      text: 'beta full text',
      has_html: false,
      has_rtf: false,
      image_url: null,
      files: null,
    })
    await open(4)
    await waitFor(() =>
      expect(ipc.called('trigger_action')).toEqual([
        { action: 'translate_selection', text: 'beta full text', origin: 'clipboard' },
      ]),
    )
  })

  it('粘贴被单飞拒绝（conflict：本次会话已经粘贴过）：静默忽略，不提示「粘贴失败」', async () => {
    ipc.results.clipboard_paste = () => {
      throw { code: 'conflict', message: 'paste already in progress', llm: null }
    }
    mount()
    key('Enter')
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(1))
    await new Promise((r) => setTimeout(r, 20))
    expect(screen.queryByTestId('clip-status')).toBeNull()
    expect(section().textContent).not.toContain(t('clipboard.status.failed.paste'))
  })

  it('面板出现时指针恰好停在某一行上（WebKit 的合成 pointermove）：不选中那一行，↩ 粘贴的是最新的一条', async () => {
    mount()
    expect(selected()).toBe('clip-row-1')
    // 指针没动，WebKit 在显示时派发一次合成的 pointermove（坐标与初始值不同）
    const row = document.getElementById('clip-row-4') as HTMLElement
    fireEvent.pointerMove(row, { clientX: 140, clientY: 160, pointerType: 'mouse' })
    expect(selected()).toBe('clip-row-1')
    key('Enter')
    await waitFor(() => expect(ipc.called('clipboard_paste')).toEqual([{ id: 1, mode: 'normal' }]))
  })

  it('指针真的移动之后才悬停选中', () => {
    mount()
    const row = document.getElementById('clip-row-4') as HTMLElement
    fireEvent.pointerMove(row, { clientX: 140, clientY: 160, pointerType: 'mouse' })
    fireEvent.pointerMove(row, { clientX: 142, clientY: 161, pointerType: 'mouse' })
    expect(selected()).toBe('clip-row-4')
  })

  it('悬停 300 ms 后显示预览卡片；原图只在这时请求', async () => {
    ipc.results.clipboard_get = ({ id }: { id: number }) => ({
      meta: sample().find((m) => m.id === id),
      text: null,
      has_html: false,
      has_rtf: false,
      image_url: 'img/5',
      files: null,
    })
    mount()
    armPointer()
    const row = document.getElementById('clip-row-5') as HTMLElement
    fireEvent.pointerMove(row, { clientX: 50, clientY: 50, pointerType: 'mouse' })
    expect(selected()).toBe('clip-row-5')
    expect(ipc.called('clipboard_get')).toHaveLength(0)
    const card = await screen.findByTestId('clip-hover', undefined, { timeout: 2000 })
    await waitFor(() => expect(ipc.called('clipboard_get')).toEqual([{ id: 5 }]))
    await waitFor(() =>
      expect(card.querySelector('img.clip-image-full')?.getAttribute('src')).toBe('wisp://img/5'),
    )
    const full = card.querySelector('img.clip-image-full') as HTMLImageElement
    // 移开：卡片消失，原图的 src 被清掉（退出预览后释放）
    fireEvent.pointerLeave(row.closest('.clip-listwrap') as HTMLElement)
    await waitFor(() => expect(screen.queryByTestId('clip-hover')).toBeNull())
    expect(full.hasAttribute('src')).toBe(false)
  })
})

describe('菜单与悬停卡片', () => {
  it('菜单里的「删除」：普通条目标 ⌫ / Delete；置顶项（菜单里直接删、不弹确认）标 ⌘⌫ / Ctrl+Delete', () => {
    const shortcut = (pinned: boolean, os: 'macos' | 'windows') => {
      const runner = testRunner([meta(1, { pinned })], os)
      return runner.menu('context', runner.ctx('context', 1), t).find((e) => e.id === 'delete')
    }
    expect(shortcut(false, 'macos')).toMatchObject({ shortcut: 'Backspace' })
    expect(shortcut(true, 'macos')).toMatchObject({ shortcut: 'Mod+Backspace' })
    expect(shortcut(false, 'windows')).toMatchObject({ shortcut: 'Delete' })
    expect(shortcut(true, 'windows')).toMatchObject({ shortcut: 'Control+Delete' })
  })

  it('悬停卡片：指针在卡片上慢下来时保持；正常速度扫过时收起（不挡指针的去路）；点卡片粘贴它对应的条目', async () => {
    ipc.results.clipboard_get = () => null
    await preloadPreview()
    mount()
    armPointer()
    const row = document.getElementById('clip-row-3') as HTMLElement
    fireEvent.pointerMove(row, { clientX: 40, clientY: 40, pointerType: 'mouse' })
    const card = await screen.findByTestId('clip-hover', undefined, { timeout: 2000 })
    await new Promise((r) => setTimeout(r, 60))
    // 慢：卡片保持，选中项不变
    fireEvent.pointerMove(card, { clientX: 41, clientY: 41, pointerType: 'mouse' })
    expect(screen.getByTestId('clip-hover')).toBe(card)
    expect(selected()).toBe('clip-row-3')
    fireEvent.click(card)
    await waitFor(() => expect(ipc.called('clipboard_paste')).toEqual([{ id: 3, mode: 'normal' }]))
    // 再悬停一次，然后快速扫过卡片：卡片收起
    fireEvent.pointerMove(row, { clientX: 42, clientY: 40, pointerType: 'mouse' })
    const again = await screen.findByTestId('clip-hover', undefined, { timeout: 2000 })
    fireEvent.pointerMove(again, { clientX: 42, clientY: 90, pointerType: 'mouse' })
    fireEvent.pointerMove(again, { clientX: 42, clientY: 130, pointerType: 'mouse' })
    await waitFor(() => expect(screen.queryByTestId('clip-hover')).toBeNull())
  })
})

describe('删除与清空的动效', () => {
  it('删除：被删的行留下残影淡出；下面的行合拢上来，指示器从新选中行此刻的位置出发、与它同一条曲线', async () => {
    setPlatform('macos', false)
    mount()
    key('ArrowDown')
    key('ArrowDown')
    expect(selected()).toBe('clip-row-3')
    await waitFor(() => expect(indicatorY()).toBeCloseTo(rowY(3), 0))
    key('Backspace')
    expect(selected()).toBe('clip-row-4')
    const ghost = document.querySelector<HTMLElement>('.clip-ghost')
    expect(ghost).not.toBeNull()
    expect(ghost?.getAttribute('aria-hidden')).toBe('true')
    expect(ghost?.querySelector('[id]')).toBeNull()
    const content = () =>
      document.getElementById('clip-row-4')?.closest('.ui-vlist-row')
        ?.firstElementChild as HTMLElement
    expect(content().style.transform).toBe(`translateY(${ROW_HEIGHT}px)`)
    const offset = () =>
      Number(/translateY\((-?[\d.]+)px\)/.exec(content().style.transform)?.[1] ?? 0)
    for (let i = 0; i < 4; i++) {
      // 逐帧采样，必须串行
      // oxlint-disable-next-line no-await-in-loop
      await oneFrame()
      expect(Math.abs(indicatorY() - (rowY(4) + offset()))).toBeLessThan(1)
    }
    await waitFor(() => expect(document.querySelector('.clip-ghost')).toBeNull())
    await waitFor(() => expect(indicatorY()).toBeCloseTo(rowY(4), 0))
  })

  it('清空（保留置顶）：被清掉的行与「最近」标题一起淡出；减少动效时直接移除', async () => {
    setPlatform('macos', false)
    mount()
    ipc.changed({ change: 'cleared', keep_pinned: true })
    expect(rowIds()).toEqual([1, 2])
    expect(document.querySelectorAll('.clip-ghost').length).toBeGreaterThan(6)
    await waitFor(() => expect(document.querySelectorAll('.clip-ghost')).toHaveLength(0))
  })
})

describe('按住 ⌘ 显示 ⌘n 角标', () => {
  it('按下 / 松开修饰键切换 data-mod-held；角标在视图显示后补上（前 9 条）', async () => {
    mount()
    await waitFor(() => expect(document.querySelectorAll('.clip-badge')).toHaveLength(8))
    act(() => {
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Meta', metaKey: true }))
    })
    expect(section().hasAttribute('data-mod-held')).toBe(true)
    act(() => {
      window.dispatchEvent(new KeyboardEvent('keyup', { key: 'Meta', metaKey: false }))
    })
    expect(section().hasAttribute('data-mod-held')).toBe(false)
  })
})

describe('错误', () => {
  it('条目已不存在（not_found）：从列表移除并提示', async () => {
    ipc.results.clipboard_pin = () => {
      throw { code: 'not_found', message: 'gone', llm: null }
    }
    mount()
    key('ArrowDown', { meta: true })
    key('KeyD', { meta: true })
    expect(await screen.findByTestId('clip-status')).toHaveTextContent(t('clipboard.status.gone'))
    expect(rowIds()).not.toContain(8)
  })

  it('其他失败：回滚乐观更新并提示', async () => {
    ipc.results.clipboard_pin = () => {
      throw { code: 'internal', message: 'boom', llm: null }
    }
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    mount()
    key('ArrowDown', { meta: true })
    key('KeyD', { meta: true })
    expect(await screen.findByTestId('clip-status')).toHaveTextContent(
      t('clipboard.status.failed.favorite'),
    )
    await waitFor(() => expect(rowIds()).toEqual([1, 2, 3, 4, 5, 6, 7, 8]))
  })

  it('错误提示停留 ≥ 4 s（不是「已保存」的 1.5 s），指针停在上面时暂停计时；停在提示条里时键位提示让位', async () => {
    ipc.results.clipboard_pin = () => {
      throw { code: 'internal', message: 'boom', llm: null }
    }
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'], shouldAdvanceTime: true })
    try {
      mount()
      key('ArrowDown', { meta: true })
      key('KeyD', { meta: true })
      const pill = await screen.findByTestId('clip-status')
      expect(screen.getByTestId('view-clipboard')).toHaveAttribute('data-status-bar')
      // 「已保存」只停 1.5 s；错误 2.5 s 后仍在
      act(() => vi.advanceTimersByTime(2500))
      expect(screen.getByTestId('clip-status')).toBe(pill)
      // 悬停暂停：再过 6 s 仍在
      fireEvent.pointerEnter(pill)
      act(() => vi.advanceTimersByTime(6000))
      expect(screen.queryByTestId('clip-status')).not.toBeNull()
      // 移开之后剩余的时间（至少 1.5 s）再走完
      fireEvent.pointerLeave(pill)
      act(() => vi.advanceTimersByTime(1000))
      expect(screen.queryByTestId('clip-status')).not.toBeNull()
      act(() => vi.advanceTimersByTime(1000))
    } finally {
      vi.useRealTimers()
    }
    await waitFor(() => expect(screen.queryByTestId('clip-status')).toBeNull())
    expect(screen.getByTestId('view-clipboard')).not.toHaveAttribute('data-status-bar')
  })

  it('轻提示停留：一眼就懂的确认 1.5 s；带说明的信息按字数计时（≥ 3 s，≤ 5 s）；错误 4 s', () => {
    expect(statusHoldMs(t('clipboard.status.saved'), 'ok')).toBe(1500)
    // 「已跳过一条疑似密码或密钥的内容」：15 个字，有隐私含义，1.5 s 读不完
    expect(statusHoldMs('已跳过一条疑似密码或密钥的内容', 'info')).toBe(3000)
    const en = 'Skipped something that looked like a password or key'
    expect(statusHoldMs(en, 'info')).toBeGreaterThan(3000)
    expect(statusHoldMs(en, 'info')).toBeLessThanOrEqual(5000)
    expect(statusHoldMs('x'.repeat(400), 'info')).toBe(5000)
    expect(statusHoldMs('boom', 'error')).toBe(4000)
  })

  it('文件条目粘贴返回 not_found（文件都已不存在，记录还在）：不移除，整行置灰并说明', async () => {
    const files = meta(9, {
      kind: 'files',
      preview: 'a.pdf',
      char_count: null,
      line_count: null,
      files: [{ path: '/tmp/a.pdf', name: 'a.pdf', exists: true, size: 10 }],
    })
    ipc.results.clipboard_paste = () => {
      throw { code: 'not_found', message: 'files no longer exist', llm: null }
    }
    mount([...sample(), files])
    key('ArrowDown', { meta: true })
    expect(selected()).toBe('clip-row-9')
    key('Enter')
    expect(await screen.findByTestId('clip-status')).toHaveTextContent(
      t('clipboard.status.files_gone'),
    )
    expect(rowIds()).toContain(9)
    expect(document.getElementById('clip-row-9')?.hasAttribute('data-missing')).toBe(true)
  })

  it('清空失败：列表恢复原样（数据仍在库里）', async () => {
    ipc.results.clipboard_clear = () => {
      throw { code: 'internal', message: 'locked', llm: null }
    }
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    mount()
    act(() => screen.getByTestId('clip-more').click())
    act(() => screen.getByTestId('clip-more-menu-item-clear').click())
    key('Enter')
    expect(await screen.findByTestId('clip-status')).toHaveTextContent(
      t('clipboard.status.failed.clear'),
    )
    await waitFor(() => expect(rowIds()).toEqual([1, 2, 3, 4, 5, 6, 7, 8]))
  })

  it('检索模式下删除失败：条目回到检索结果里', async () => {
    ipc.results.clipboard_search = ({ query, filter }: { query: string; filter: string }) => ({
      items: sample()
        .filter((m) => m.preview.includes(query))
        .map((item) => ({ item, highlights: [] })),
      pinned_count: 0,
      next_offset: null,
      query,
      filter,
    })
    ipc.results.clipboard_delete = () => {
      throw { code: 'internal', message: 'busy', llm: null }
    }
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    mount()
    fireEvent.change(search(), { target: { value: 'eta' } })
    await waitFor(() => expect(rowIds()).toEqual([4]))
    key('Backspace', { meta: true })
    await waitFor(() => expect(screen.getByTestId('clip-status')).toBeTruthy())
    await waitFor(() => expect(rowIds()).toEqual([4]))
  })
})

describe('行（分组与无障碍）', () => {
  it('搜索框是组合框：aria-controls 指向 listbox 本身，aria-expanded；选项名称里的时间用完整写法', () => {
    mount()
    expect(search().getAttribute('role')).toBe('combobox')
    expect(search().getAttribute('aria-expanded')).toBe('true')
    const listbox = screen.getByTestId('clip-list')
    expect(listbox.getAttribute('role')).toBe('listbox')
    expect(search().getAttribute('aria-controls')).toBe(listbox.id)
    const item = meta(3, { last_used_at: NOW - 3 * 86_400_000 })
    const label = rowLabel(item, NOW, t)
    expect(label).toContain(relativeTime(item.last_used_at, NOW, 'long'))
    expect(label).not.toContain(relativeTime(item.last_used_at, NOW, 'short'))
  })

  it('选项的可访问名称：类型 · 内容 · 来源 · 时间（· 已收藏）；⌘n 在 aria-keyshortcuts，角标不进名称', () => {
    mount()
    const row1 = document.getElementById('clip-row-1') as HTMLElement
    const name = row1.getAttribute('aria-label') ?? ''
    expect(name).toContain(t('clipboard.kind.text'))
    expect(name).toContain('收货地址')
    expect(name).toContain(t('clipboard.a11y.favorite'))
    expect(row1.getAttribute('aria-keyshortcuts')).toBe('Meta+1 Alt+Meta+1')
    // 图片：尺寸与大小之间有分隔
    const img = document.getElementById('clip-row-5')?.getAttribute('aria-label') ?? ''
    expect(img).toMatch(/800 × 600, \S/)
    // 分组标题不是选项
    expect(document.querySelector('.clip-group')?.getAttribute('role')).toBe('presentation')
  })

  it('只有空白的正文显示占位说明（不是一行空白）；文件都不存在时标「已不存在」', () => {
    const files = meta(9, {
      kind: 'files',
      preview: 'gone.dmg',
      char_count: null,
      line_count: null,
      files: [{ path: '/tmp/gone.dmg', name: 'gone.dmg', exists: false, size: null }],
    })
    mount([meta(1, { preview: '', char_count: 3 }), files])
    expect(document.getElementById('clip-row-1')?.textContent).toContain(
      t('clipboard.meta.blank', { count: 3, n: '3' }),
    )
    expect(document.getElementById('clip-row-9')?.textContent).toContain(
      t('clipboard.meta.missing'),
    )
  })

  it('提示条：五项「粘贴 · 纯文本 · 仅复制 · 收藏 · 删除」在前，之后是 Space 预览、⌘K 操作、⌘F 筛选与 Esc 关闭', () => {
    mount()
    const ids = [...screen.getByTestId('clip-hints').children].map((c) =>
      c.getAttribute('data-hint'),
    )
    expect(ids).toEqual([
      'paste',
      'paste_plain',
      'copy_only',
      'favorite',
      'delete',
      'preview',
      'open_actions',
      'open_filter_menu',
      'escape',
    ])
  })
})

describe('预览', () => {
  it('预取完成后第一次按 Space：同一次提交里渲染预览（没有 Suspense 的挂起与空白帧）', async () => {
    ipc.results.clipboard_get = () => null
    await preloadPreview()
    mount()
    key('Space')
    // 不等待：按键的这一次提交里预览就在，列表同时开始淡出
    expect(screen.getByTestId('clip-preview')).toBeTruthy()
    expect(document.querySelector('.clip-listwrap')?.hasAttribute('data-covered')).toBe(true)
  })

  it('↑↓ 换条目：等宽开关回到新条目的默认值（代码等宽、文本不等宽）', async () => {
    ipc.results.clipboard_get = () => null
    await preloadPreview()
    const items = [meta(1, { kind: 'code', preview: 'const a = 1' }), meta(2, { preview: 'plain' })]
    mount(items)
    key('Space')
    const mono = () => screen.getByTestId('clip-preview-monospace').getAttribute('aria-pressed')
    expect(mono()).toBe('true')
    key('ArrowDown')
    expect(mono()).toBe('false')
    key('ArrowUp')
    expect(mono()).toBe('true')
  })

  it('悬停单行文本：直接用元数据里的预览，不取正文；多行文本只取一次（开头缓存）', async () => {
    ipc.results.clipboard_get = ({ id }: { id: number }) => ({
      meta: sample().find((m) => m.id === id),
      text: `${'x'.repeat(100_000)}\nsecond`,
      has_html: false,
      has_rtf: false,
      image_url: null,
      files: null,
    })
    await preloadPreview()
    const items = [meta(1, { preview: 'one line', char_count: 8 }), meta(2, { line_count: 4 })]
    mount(items)
    armPointer()
    const hover = async (id: number) => {
      const row = document.getElementById(`clip-row-${id}`) as HTMLElement
      fireEvent.pointerMove(row, { clientX: 20 + id, clientY: 20 + id, pointerType: 'mouse' })
      await screen.findByTestId('clip-hover', undefined, { timeout: 2000 })
    }
    await hover(1)
    expect(ipc.called('clipboard_get')).toHaveLength(0)
    await hover(2)
    await waitFor(() => expect(ipc.called('clipboard_get')).toHaveLength(1))
    // 每行截到 300 字
    await waitFor(() =>
      expect(
        (
          screen.getAllByTestId('clip-hover').at(-1)?.querySelector('.clip-hover-text')
            ?.textContent ?? ''
        ).length,
      ).toBeLessThan(400),
    )
    await hover(1)
    await hover(2)
    expect(ipc.called('clipboard_get')).toHaveLength(1)
  })
})

describe('Wayland', () => {
  it('平台层要求由 WebView 写剪贴板：写入后在原位切换到 toast 视图', async () => {
    ipc.results.clipboard_paste = () => {
      throw { code: 'platform', message: 'errors.platform.wayland_clipboard', llm: null }
    }
    ipc.results.clipboard_get = () => ({
      meta: sample()[2],
      text: 'alpha full',
      has_html: false,
      has_rtf: false,
      image_url: null,
      files: null,
    })
    const writeText = vi.fn<(text: string) => Promise<void>>(() => Promise.resolve())
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText } })
    setPlatform('linux')
    const { props } = mount()
    key('ArrowDown')
    key('ArrowDown')
    key('Enter')
    await waitFor(() => expect(props.switchView).toHaveBeenCalled())
    expect(writeText).toHaveBeenCalledWith('alpha full')
    expect(props.switchView).toHaveBeenCalledWith(
      { kind: 'toast', reason: 'wayland', paste_key: 'Ctrl+V' },
      { width: expect.any(Number) },
    )
    // 之后的 Ctrl+Enter（仅复制）同样由 WebView 写入，toast 只说「已复制」
    key('Enter', { ctrl: true })
    await waitFor(() => expect(props.switchView).toHaveBeenCalledTimes(2))
    expect(props.switchView).toHaveBeenLastCalledWith(
      { kind: 'toast', reason: 'copy_only', paste_key: 'Ctrl+V' },
      { width: expect.any(Number) },
    )
    vi.unstubAllGlobals()
  })
})

describe('生命周期（隐藏时释放）', () => {
  it('clipboard:changed 是常驻订阅：卸载（面板隐藏）只解绑视图，再挂载不重复订阅', async () => {
    const first = mount()
    await waitFor(() => expect(ipc.listeners.get('clipboard:changed')?.size).toBe(1))
    first.unmount()
    // 订阅留着（下一次唤出时 ready 之后补发的事件不会先于订阅到达），卸载的视图不再收到事件
    expect(ipc.listeners.get('clipboard:changed')?.size).toBe(1)
    expect(() => ipc.changed({ change: 'removed', id: 1 })).not.toThrow()
    mount()
    expect(ipc.listeners.get('clipboard:changed')?.size).toBe(1)
    // 新挂载的视图照常收到事件
    ipc.changed({ change: 'removed', id: 2 })
    await waitFor(() => expect(document.getElementById('clip-row-2')).toBeNull())
  })
})

describe('JIT 预热', () => {
  it('在暂存节点里渲染一屏条目再卸载：不发 IPC（不取详情 / 缩略图、不加载下一页）、不留节点', async () => {
    const calls = ipc.calls.length
    prewarmClipboardRender()
    // 真的渲染了一棵视图（预热会吞掉异常：渲染失败时测试要能发现）
    expect(lastPrewarm.error).toBeNull()
    expect(lastPrewarm.nodes).toBeGreaterThan(10)
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20))
    })
    expect(ipc.calls.length).toBe(calls)
    expect(document.querySelector('[data-testid="view-clipboard"]')).toBeNull()
  })
})

describe('记录状态', () => {
  it('历史库来自更新版本、列表为空：列表区说明原因，不显示「复制点什么」的空状态', () => {
    mount([], { unavailable: 'clipboard.history_too_new' })
    const body = screen.getByTestId('clip-unavailable')
    expect(body).toHaveTextContent(t('clipboard.recording.unavailable_title'))
    expect(body).toHaveTextContent(t('clipboard.history_too_new'))
    expect(screen.queryByTestId('clip-empty')).toBeNull()
  })

  it('设置里关掉了剪贴板监听：横幅说明新的复制不会被记录，[打开设置] 打开剪贴板设置', async () => {
    mount(sample(), { recording: { state: 'disabled' } })
    const banner = screen.getByTestId('clip-disabled')
    expect(banner).toHaveTextContent(t('clipboard.recording.disabled'))
    fireEvent.click(within(banner).getByRole('button'))
    await waitFor(() =>
      expect(ipc.called('window_open')).toEqual([
        { target: { kind: 'settings', section: 'clipboard' } },
      ]),
    )
  })

  it('监听装不上（X11 缺 XFIXES / Wayland 没有 data-control）：横幅说明新的复制不会被记录，具体原因在提示里', () => {
    mount(sample(), {
      recording: {
        state: 'unavailable',
        reason_key: 'errors.platform.clipboard_watch_unavailable',
      },
    })
    const banner = screen.getByTestId('clip-watch-unavailable')
    expect(banner).toHaveTextContent(t('clipboard.watch_unavailable'))
    expect(banner.querySelector('[title]')?.getAttribute('title')).toBe(
      t('errors.platform.clipboard_watch_unavailable'),
    )
  })
})

describe('拖动（「记住位置」）', () => {
  const down = (el: Element) => fireEvent.pointerDown(el, { button: 0, pointerType: 'mouse' })

  it('从头部与提示条的空白处拖动（共用 usePanelDrag）；搜索框、条目、按钮不触发，焦点留在搜索框', () => {
    mount(sample(), { hint_bar: true })
    const head = section().querySelector('.clip-head')
    if (!head) throw new Error('no header')
    down(head)
    expect(ipc.called('panel_drag_start')).toEqual([{ session_id: 7 }])
    expect(fireEvent.mouseDown(head, { button: 0 })).toBe(false)
    expect(document.activeElement).toBe(search())
    down(screen.getByTestId('clip-hints'))
    expect(ipc.called('panel_drag_start')).toHaveLength(2)
    down(search())
    down(screen.getAllByTestId('clip-row')[0] as HTMLElement)
    for (const button of within(head as HTMLElement).queryAllByRole('button')) down(button)
    expect(ipc.called('panel_drag_start')).toHaveLength(2)
  })
})
