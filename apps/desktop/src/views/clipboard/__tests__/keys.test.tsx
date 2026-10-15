/**
 * 剪贴板键位表逐键测试：
 * ↑↓、⌘↑/↓、PageUp/Down、↩ / ⇧↩ / ⌘↩、⌘1–9 / 纯文本 ⌥⌘1–9（Windows / Linux Ctrl+Shift+1–9）、⌘P、⌫（搜索为空时；置顶项二次确认）/ ⌘⌫、Space（搜索为空时）
 * / ⇧Space、Tab / ⇧Tab、⌘F、Esc（先清空再交给外壳关闭）；Windows / Linux 上 ⌘→Ctrl、⌫→Delete；
 * 搜索框有字时 ⌫ 是退格、Space 是空格；Tab 不会移走焦点；输入法组合中不触发。
 */
import { act, fireEvent, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { setTransport } from '../../../shared/ipc'
import { t } from '../../../shared/i18n/i18n'
import { ClipboardView } from '../ClipboardView'
import {
  armPointer,
  fakeIpc,
  idleTransport,
  key,
  meta,
  payload,
  renderIn,
  restorePlatform,
  sample,
  setPlatform,
  viewProps,
  type FakeIpc,
} from './helpers'

let ipc: FakeIpc

function mount(items = sample(), extra = {}) {
  const props = viewProps<'clipboard'>(payload(items, extra))
  renderIn(<ClipboardView {...props} />)
  return props
}

const search = () => screen.getByTestId('clip-search') as HTMLInputElement
/** 过滤 Chip 上当前显示的类型名（所有类型名叠在同一格里，只有当前的可见） */
const filterLabel = () =>
  screen.getByTestId('clip-filter').querySelector('[data-on]')?.textContent ?? ''
const selected = () => search().getAttribute('aria-activedescendant')
const rowId = (id: number) => `clip-row-${id}`

beforeEach(() => {
  ipc = fakeIpc()
  ipc.results.clipboard_paste = { type: 'pasted' }
  ipc.results.clipboard_pin = ({ id, pinned }: { id: number; pinned: boolean }) =>
    meta(id, { ...sample().find((m) => m.id === id), pinned })
  ipc.results.clipboard_recent = ({ filter }: { filter: string }) => ({
    items: [],
    pinned_count: 0,
    next_offset: null,
    query: null,
    filter,
  })
  ipc.results.clipboard_search = ({ query, filter }: { query: string; filter: string }) => ({
    items: sample()
      .filter((m) => m.preview.includes(query))
      .map((item) => ({ item, highlights: [] })),
    pinned_count: 0,
    next_offset: null,
    query,
    filter,
  })
  setPlatform('macos')
})

afterEach(() => {
  restorePlatform()
  setTransport(idleTransport())
})

describe('移动（↑↓ / ⌘↑↓ / PageUp / PageDown）', () => {
  it('↓ / ↑ 移动选中项并跳过分组标题；按住连续移动', () => {
    mount()
    expect(selected()).toBe(rowId(1))
    key('ArrowDown')
    expect(selected()).toBe(rowId(2))
    // 2 → 3 中间隔着「今天」标题
    key('ArrowDown', { repeat: true })
    expect(selected()).toBe(rowId(3))
    key('ArrowUp')
    expect(selected()).toBe(rowId(2))
    key('ArrowUp')
    key('ArrowUp')
    expect(selected()).toBe(rowId(1))
  })

  it('⌘↓ 跳到最后一条，⌘↑ 回到第一条', () => {
    mount()
    key('ArrowDown', { meta: true })
    expect(selected()).toBe(rowId(8))
    key('ArrowUp', { meta: true })
    expect(selected()).toBe(rowId(1))
  })

  it('PageDown / PageUp 按一屏移动', () => {
    mount()
    key('PageDown')
    expect(selected()).toBe(rowId(8))
    key('PageUp')
    expect(selected()).toBe(rowId(1))
  })
})

describe('粘贴（↩ / ⇧↩ / ⌘↩ / ⌘1–9 / ⌥⌘1–9）', () => {
  it('↩ 粘贴、⇧↩ 纯文本、⌘↩ 仅复制（作用于选中项）', async () => {
    mount()
    key('ArrowDown')
    key('Enter')
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(1))
    key('Enter', { shift: true })
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(2))
    key('Enter', { meta: true })
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(3))
    expect(ipc.called('clipboard_paste')).toEqual([
      { id: 2, mode: 'normal' },
      { id: 2, mode: 'plain' },
      { id: 2, mode: 'copy_only' },
    ])
  })

  it('⌘n 按当前可见顺序（含置顶区）粘贴第 n 条；macOS 的纯文本是 ⌥⌘n（⌘⇧3–5 是系统截屏键）', async () => {
    mount()
    key('Digit3', { meta: true })
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(1))
    // ⌘⇧n 在 macOS 上不再绑定
    key('Digit2', { meta: true, shift: true, key: '@' })
    key('Digit2', { meta: true, alt: true, key: '™' })
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(2))
    expect(ipc.called('clipboard_paste')).toEqual([
      { id: 3, mode: 'normal' },
      { id: 2, mode: 'plain' },
    ])
    // 超出条目数的数字什么都不做
    key('Digit9', { meta: true })
    expect(ipc.called('clipboard_paste')).toHaveLength(2)
  })

  it('⌘n 的计数受过滤与搜索影响', async () => {
    mount()
    fireEvent.change(search(), { target: { value: 'ta' } })
    // beta / delta
    await waitFor(() => expect(screen.getAllByTestId('clip-row')).toHaveLength(2))
    key('Digit2', { meta: true })
    await waitFor(() => expect(ipc.called('clipboard_paste')).toEqual([{ id: 7, mode: 'normal' }]))
  })

  it('按住 ↩ 的重复事件不会重复粘贴；输入法组合中的 ↩ 不触发', async () => {
    mount()
    key('Enter', { repeat: true })
    key('Enter', { composing: true })
    await act(async () => {})
    expect(ipc.called('clipboard_paste')).toHaveLength(0)
  })
})

describe('置顶与删除（⌘P / ⌫ / ⌘⌫）', () => {
  it('⌘P 置顶 / 取消置顶', async () => {
    mount()
    key('ArrowDown', { meta: true })
    key('KeyP', { meta: true })
    await waitFor(() => expect(ipc.called('clipboard_pin')).toEqual([{ id: 8, pinned: true }]))
    // 乐观更新：条目进入置顶区，选中项跟随
    expect(selected()).toBe(rowId(8))
  })

  it('搜索框为空时 ⌫ 删除普通条目', async () => {
    mount()
    key('ArrowDown', { meta: true })
    key('Backspace')
    await waitFor(() => expect(ipc.called('clipboard_delete')).toEqual([{ id: 8, force: false }]))
    expect(screen.queryByTestId(rowId(8))).toBeNull()
    expect(selected()).toBe(rowId(7))
  })

  it('置顶项：第一次 ⌫ 只弹出行内确认，再按一次才删除', async () => {
    mount()
    key('Backspace')
    expect(ipc.called('clipboard_delete')).toHaveLength(0)
    expect(screen.getByTestId('clip-confirm-delete')).toBeTruthy()
    key('Backspace')
    await waitFor(() => expect(ipc.called('clipboard_delete')).toEqual([{ id: 1, force: true }]))
  })

  it('⌘⌫ 直接删除（置顶项也不确认；搜索框有字时同样生效）', async () => {
    mount()
    fireEvent.change(search(), { target: { value: 'x' } })
    key('Backspace', { meta: true })
    await waitFor(() => expect(ipc.called('clipboard_delete')).toEqual([{ id: 1, force: true }]))
  })

  it('搜索框有字时 ⌫ 是退格（不拦截、不删除）', () => {
    mount()
    fireEvent.change(search(), { target: { value: 'abc' } })
    const ev = key('Backspace')
    expect(ev.defaultPrevented).toBe(false)
    expect(ipc.called('clipboard_delete')).toHaveLength(0)
  })
})

describe('预览（Space / ⇧Space）', () => {
  it('搜索框为空时 Space 打开 / 关闭全文预览', async () => {
    ipc.results.clipboard_get = ({ id }: { id: number }) => ({
      meta: sample().find((m) => m.id === id),
      text: 'full text',
      has_html: false,
      has_rtf: false,
      image_url: null,
      files: null,
    })
    mount()
    key('Space')
    expect(await screen.findByTestId('clip-preview')).toBeTruthy()
    key('Space')
    await waitFor(() => expect(screen.queryByTestId('clip-preview')).toBeNull())
  })

  it('搜索框有字时 Space 是空格，⇧Space 打开预览', async () => {
    mount()
    fireEvent.change(search(), { target: { value: 'a' } })
    const ev = key('Space')
    expect(ev.defaultPrevented).toBe(false)
    expect(screen.queryByTestId('clip-preview')).toBeNull()
    key('Space', { shift: true })
    expect(await screen.findByTestId('clip-preview')).toBeTruthy()
  })
})

describe('过滤（Tab / ⇧Tab / ⌘F）', () => {
  it('Tab / ⇧Tab 循环切换类型，焦点不会移走', () => {
    mount()
    search().focus()
    const ev = key('Tab')
    expect(ev.defaultPrevented).toBe(true)
    expect(filterLabel()).toBe(t('clipboard.filter.text'))
    expect(document.activeElement).toBe(search())
    key('Tab', { shift: true })
    key('Tab', { shift: true })
    expect(filterLabel()).toBe(t('clipboard.filter.color'))
  })

  it('⌘F 打开类型过滤菜单（勾选当前项）', async () => {
    mount()
    key('KeyF', { meta: true })
    const menu = await screen.findByTestId('clip-filter-menu')
    expect(menu.getAttribute('role')).toBe('menu')
    expect(menu.querySelector('[aria-checked="true"]')?.textContent).toContain(
      t('clipboard.filter.all'),
    )
  })
})

describe('Esc', () => {
  it('先清空搜索，再交给外壳关闭面板', () => {
    mount()
    fireEvent.change(search(), { target: { value: 'abc' } })
    const first = key('Escape')
    expect(first.defaultPrevented).toBe(true)
    expect(search().value).toBe('')
    const second = key('Escape')
    // 视图层没有处理：路由继续交给外壳的全局层（这里没有外壳，事件保持未处理）
    expect(second.defaultPrevented).toBe(false)
  })

  it('删除确认打开时先取消确认', () => {
    mount()
    key('Backspace')
    expect(screen.getByTestId('clip-confirm-delete')).toBeTruthy()
    const ev = key('Escape')
    expect(ev.defaultPrevented).toBe(true)
  })
})

describe('Windows / Linux：⌘ → Ctrl、⌫ → Delete', () => {
  it('Delete 删除、Ctrl+Delete 强制删除；Backspace 不删除；Ctrl+1 粘贴', async () => {
    setPlatform('windows')
    mount()
    key('ArrowDown', { ctrl: true })
    key('Backspace')
    expect(ipc.called('clipboard_delete')).toHaveLength(0)
    key('Delete')
    await waitFor(() => expect(ipc.called('clipboard_delete')).toEqual([{ id: 8, force: false }]))
    key('Delete', { ctrl: true })
    await waitFor(() => expect(ipc.called('clipboard_delete')).toHaveLength(2))
    // force 只对置顶项有意义（Rust 要求置顶项带 force）；普通条目直接删除
    expect(ipc.called('clipboard_delete')[1]).toEqual({ id: 7, force: false })
    key('Digit1', { ctrl: true })
    await waitFor(() => expect(ipc.called('clipboard_paste')).toEqual([{ id: 1, mode: 'normal' }]))
    // macOS 的 ⌘ 在 Windows 上不是修饰键
    key('Digit2', { meta: true })
    expect(ipc.called('clipboard_paste')).toHaveLength(1)
    // 纯文本快速粘贴在 Windows / Linux 仍是 Ctrl+Shift+n（没有截屏键冲突）
    key('Digit2', { ctrl: true, shift: true, key: '@' })
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(2))
    expect(ipc.called('clipboard_paste')[1]).toEqual({ id: 2, mode: 'plain' })
  })
})

describe('按住不放（按键重复）', () => {
  it('按住 Space：只打开一次预览（不会开了又关），重复事件被拦下、不会把空格打进搜索框', async () => {
    ipc.results.clipboard_get = () => null
    const { preloadPreview } = await import('../previewModule')
    await preloadPreview()
    mount()
    search().focus()
    const view = () => screen.getByTestId('view-clipboard')
    key('Space')
    expect(view().hasAttribute('data-preview')).toBe(true)
    for (let i = 0; i < 5; i++) {
      const ev = key('Space', { repeat: true })
      expect(ev.defaultPrevented).toBe(true)
    }
    expect(view().hasAttribute('data-preview')).toBe(true)
    expect(search().value).toBe('')
    // 松开再按一次：关闭；之后搜索框为空时 ⌫ / Space 仍是视图快捷键
    key('Space')
    expect(view().hasAttribute('data-preview')).toBe(false)
    expect(search().value).toBe('')
  })

  it('按住 ⌫ / ⌘P / ↩：只执行一次，重复事件同样被拦下', async () => {
    mount()
    key('ArrowDown', { meta: true })
    const first = key('Backspace')
    expect(first.defaultPrevented).toBe(true)
    expect(key('Backspace', { repeat: true }).defaultPrevented).toBe(true)
    expect(key('Backspace', { repeat: true }).defaultPrevented).toBe(true)
    await waitFor(() => expect(ipc.called('clipboard_delete')).toHaveLength(1))
    key('KeyP', { meta: true })
    expect(key('KeyP', { meta: true, repeat: true }).defaultPrevented).toBe(true)
    await waitFor(() => expect(ipc.called('clipboard_pin')).toHaveLength(1))
  })
})

describe('纯键盘到达全部操作', () => {
  it('⌘K 在选中行打开操作菜单（第一项高亮）：含右键菜单的全部操作与清空历史', async () => {
    mount()
    key('ArrowDown')
    key('KeyK', { meta: true })
    const menu = await screen.findByTestId('clip-context-menu')
    expect(menu.querySelector('[data-active]')?.textContent).toContain(t('clipboard.action.paste'))
    for (const id of ['paste', 'pin', 'delete', 'pause', 'settings', 'clear']) {
      expect(screen.getByTestId(`clip-context-menu-item-${id}`)).toBeTruthy()
    }
  })

  it('⇧F10 / ContextMenu 键同样打开；没有条目时打开 ⋯ 菜单', async () => {
    mount()
    key('F10', { shift: true })
    expect(await screen.findByTestId('clip-context-menu')).toBeTruthy()
  })

  it('空列表时 ⌘K 打开 ⋯ 菜单（暂停监听 / 设置 / 清空历史），破坏性的「清空历史」在最后', async () => {
    mount([])
    key('KeyK', { meta: true })
    const menu = await screen.findByTestId('clip-more-menu')
    const items = [...menu.querySelectorAll('[role="menuitem"]')].map((el) =>
      el.getAttribute('data-testid'),
    )
    expect(items).toEqual([
      'clip-more-menu-item-pause',
      'clip-more-menu-item-settings',
      'clip-more-menu-item-clear',
    ])
  })

  it('预览打开时 Tab / ⇧Tab 在预览的按钮之间移动焦点，不切换过滤；Esc 关闭后焦点回到搜索框', async () => {
    ipc.results.clipboard_get = () => null
    const { preloadPreview } = await import('../previewModule')
    await preloadPreview()
    mount()
    search().focus()
    key('Space')
    const preview = screen.getByTestId('clip-preview')
    key('Tab')
    expect(filterLabel()).toBe(t('clipboard.filter.all'))
    expect(preview.contains(document.activeElement)).toBe(true)
    expect(document.activeElement?.tagName).toBe('BUTTON')
    // 焦点在按钮上：Space 交给按钮本身（不关闭预览）
    const onButton = key('Space')
    expect(onButton.defaultPrevented).toBe(false)
    key('Tab', { shift: true })
    expect(document.activeElement).toBe(search())
    key('Tab')
    key('Escape')
    expect(document.activeElement).toBe(search())
  })
})

describe('确认条', () => {
  it('清空确认是模态的：↓ / ⌫ / 字母都不生效，Space 切换「保留置顶项」，↩ 确认', async () => {
    ipc.results.clipboard_clear = 0
    mount()
    act(() => screen.getByTestId('clip-more').click())
    act(() => screen.getByTestId('clip-more-menu-item-clear').click())
    expect(await screen.findByTestId('clip-confirm-clear')).toBeTruthy()
    const before = selected()
    key('ArrowDown')
    expect(selected()).toBe(before)
    expect(key('Backspace').defaultPrevented).toBe(true)
    expect(ipc.called('clipboard_delete')).toHaveLength(0)
    expect(key('KeyA', { key: 'a' }).defaultPrevented).toBe(true)
    expect(screen.getByTestId('view-clipboard').hasAttribute('data-preview')).toBe(false)
    const keep = () => screen.getByTestId('clip-keep-pinned') as HTMLInputElement
    expect(keep().checked).toBe(true)
    key('Space')
    expect(keep().checked).toBe(false)
    key('Enter')
    await waitFor(() => expect(ipc.called('clipboard_clear')).toEqual([{ keep_pinned: false }]))
  })

  it('删除置顶项的确认期间，再按 ⌫ 只作用于发起确认的那一条（悬停到别的行也不改）', async () => {
    mount()
    key('Backspace')
    expect(screen.getByTestId('clip-confirm-delete')).toBeTruthy()
    const other = document.getElementById(rowId(4)) as HTMLElement
    armPointer()
    fireEvent.pointerMove(other, { clientX: 10, clientY: 200, pointerType: 'mouse' })
    expect(selected()).toBe(rowId(1))
    key('Backspace')
    await waitFor(() => expect(ipc.called('clipboard_delete')).toEqual([{ id: 1, force: true }]))
  })
})
