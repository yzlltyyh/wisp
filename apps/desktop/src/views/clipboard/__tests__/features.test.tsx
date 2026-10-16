/**
 * 剪贴板 v2 的功能（都经操作注册表）：
 * - 收藏：分组标题折叠（写回 Rust、序号只数列出来的行）、超过 5 条时「查看全部 N 个收藏」进入收藏视图、⌘D、命名条；
 * - 芯片：光标停下来之后检视，芯片条出现；点芯片粘贴片段、⌥ 点击只复制；⌘E 进入芯片条，←/→ / ↩ / Esc；
 * - 「粘贴为…」：⌥⇧↩ 直接打开到子页，选一种变换按它粘贴；
 * - 多选：⌘ 点击编号、⇧↓ 扩展、↩ 合并粘贴、⌘D / ⌫ 作用于全部（含收藏时先确认）、Esc 先取消选择；
 * - 验证码的「N 分钟后删除」、图片「识别文字」、⌘P 在悬浮窗接入之前只拦下。
 */
import { act, fireEvent, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport } from '../../../shared/ipc'
import type { ClipInspect, ClipItemMeta, SettingsSnapshot } from '../../../shared/ipc/types'
import { t } from '../../../shared/i18n/i18n'
import { ClipboardView } from '../ClipboardView'
import { INSPECT_DEBOUNCE_MS } from '../model'
import { preloadPreview } from '../previewModule'
import {
  awaitClipboardView,
  installClipboardEvents,
  resetWebviewClipboardForTest,
} from '../useClipboard'
import {
  fakeIpc,
  HOUR,
  idleTransport,
  key,
  meta,
  NOW,
  payload,
  renderIn,
  restorePlatform,
  sample,
  setPlatform,
  viewProps,
  type FakeIpc,
} from './helpers'

let ipc: FakeIpc

function mount(items: ClipItemMeta[] = sample(), extra = {}) {
  const p = viewProps<'clipboard'>(payload(items, extra))
  return { props: p, ...renderIn(<ClipboardView {...p} />) }
}

const search = () => screen.getByTestId('clip-search') as HTMLInputElement
const rowIds = () =>
  screen.getAllByTestId('clip-row').map((r) => Number(r.getAttribute('data-clip-id')))
const row = (id: number) => document.getElementById(`clip-row-${id}`) as HTMLElement
/** 等检视的去抖与 IPC 往返（真实计时器） */
const settleInspect = () =>
  act(() => new Promise<void>((r) => setTimeout(r, INSPECT_DEBOUNCE_MS + 20)))

/** 7 条收藏 + 3 条最近 */
function favorites(): ClipItemMeta[] {
  return [
    ...Array.from({ length: 7 }, (_, i) =>
      meta(i + 1, {
        pinned: true,
        last_used_at: NOW - (i + 1) * HOUR,
        preview: `fav ${i + 1}`,
        name: i === 0 ? '家里地址' : null,
      }),
    ),
    meta(20, { preview: 'recent a' }),
    meta(21, { preview: 'recent b' }),
    meta(22, { preview: 'recent c' }),
  ]
}

const OTP: ClipInspect = {
  entities: [
    { kind: 'otp', value: '482913', start: 9, end: 15, sensitive: false },
    { kind: 'phone', value: '13800138000', start: 25, end: 38, sensitive: false },
  ],
  transforms: ['trim', 'upper'],
}

beforeEach(async () => {
  ipc = fakeIpc()
  ipc.results.clipboard_paste = { type: 'pasted' }
  ipc.results.clipboard_paste_text = { type: 'pasted' }
  ipc.results.clipboard_paste_many = { type: 'pasted' }
  ipc.results.clipboard_pin = ({ id, pinned }: { id: number; pinned: boolean }) =>
    meta(id, { pinned })
  setPlatform('macos')
  // 芯片条与命名条在预览块里：预取完成后同步可用
  await preloadPreview()
})

afterEach(() => {
  restorePlatform()
  setTransport(idleTransport())
  resetWebviewClipboardForTest()
  vi.useRealTimers()
})

describe('收藏', () => {
  it('「全部」里收藏只列前 5 条，末尾是「查看全部 7 个收藏」；点它进入收藏视图（平铺，没有分组标题）', async () => {
    ipc.results.clipboard_recent = ({ filter }: { filter: string }) => ({
      items: favorites()
        .filter((m) => filter !== 'favorite' || m.pinned)
        .map((item) => ({ item, highlights: [] })),
      pinned_count: 7,
      next_offset: null,
      query: null,
      filter,
    })
    mount(favorites())
    expect(rowIds()).toEqual([1, 2, 3, 4, 5, 20, 21, 22])
    const more = screen.getByTestId('clip-show-favorites')
    expect(more.textContent).toBe(t('clipboard.group.more', { n: 7 }))
    // ⌘n 只数列出来的行：第 6 条是第一条最近
    expect(row(20).getAttribute('aria-keyshortcuts')).toContain('Meta+6')
    fireEvent.click(more)
    await waitFor(() =>
      expect(ipc.called('clipboard_recent')).toContainEqual({
        offset: 0,
        limit: 200,
        filter: 'favorite',
      }),
    )
    await waitFor(() => expect(rowIds()).toEqual([1, 2, 3, 4, 5, 6, 7]))
    expect(document.querySelector('.clip-group')).toBeNull()
    expect(screen.getByTestId('clip-filter').querySelector('[data-on]')?.textContent).toBe(
      t('clipboard.filter.favorite'),
    )
  })

  it('分组标题是折叠按钮：折叠后只留标题与条数、写回 Rust；光标不落在藏起来的收藏上', () => {
    ipc.results.clipboard_set_favorites_collapsed = undefined
    mount(favorites())
    const toggle = screen.getByTestId('clip-favorites-toggle')
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    fireEvent.click(toggle)
    expect(ipc.called('clipboard_set_favorites_collapsed')).toEqual([{ collapsed: true }])
    expect(rowIds()).toEqual([20, 21, 22])
    const header = screen.getByTestId('clip-favorites-toggle')
    expect(header.getAttribute('aria-expanded')).toBe('false')
    expect(header.textContent).toContain('7')
    expect(search().getAttribute('aria-activedescendant')).toBe('clip-row-20')
    // ⌘1 粘贴的是第一条列出来的
    key('Digit1', { meta: true })
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 20, mode: 'normal' }])
  })

  it('载荷里折叠：打开时就折叠，光标在第一条最近；⋯ 菜单里「展开收藏」', async () => {
    mount(favorites(), { favorites_collapsed: true })
    expect(rowIds()).toEqual([20, 21, 22])
    expect(search().getAttribute('aria-activedescendant')).toBe('clip-row-20')
    fireEvent.click(screen.getByTestId('clip-more'))
    const item = await screen.findByTestId('clip-more-menu-item-favorites_toggle')
    expect(item.textContent).toContain(t('clipboard.action.expand_favorites'))
    fireEvent.click(item)
    expect(ipc.called('clipboard_set_favorites_collapsed')).toEqual([{ collapsed: false }])
    expect(rowIds()).toEqual([1, 2, 3, 4, 5, 20, 21, 22])
  })

  it('收藏的名称加粗显示在预览前面，读屏名称里也有', () => {
    mount(favorites())
    expect(row(1).querySelector('.clip-name')?.textContent).toBe('家里地址')
    expect(row(1).getAttribute('aria-label')).toContain('家里地址')
    expect(row(1).getAttribute('aria-label')).toContain(t('clipboard.a11y.favorite'))
  })

  it('命名：⌘K 里「命名…」打开命名条（输入框获得焦点），↩ 保存 = clipboard_set_name，命名即收藏', async () => {
    ipc.results.clipboard_set_name = ({ id, name }: { id: number; name: string | null }) =>
      meta(id, { pinned: true, name, preview: 'alpha', last_used_at: NOW - 60_000 })
    mount()
    key('ArrowDown')
    key('ArrowDown') // 3 alpha
    key('KeyK', { meta: true })
    fireEvent.click(await screen.findByTestId('clip-context-menu-item-rename'))
    const input = (await screen.findByTestId('clip-rename-input')) as HTMLInputElement
    await waitFor(() => expect(document.activeElement).toBe(input))
    fireEvent.change(input, { target: { value: '  常用  ' } })
    // 列表的快捷键在命名期间不生效：↩ 只保存
    key('Enter', { target: input })
    expect(ipc.called('clipboard_set_name')).toEqual([{ id: 3, name: '常用' }])
    expect(ipc.called('clipboard_paste')).toEqual([])
    await waitFor(() => expect(row(3).querySelector('.clip-name')?.textContent).toBe('常用'))
    // 进入收藏分组
    expect(rowIds().indexOf(3)).toBeLessThan(rowIds().indexOf(4))
  })

  it('命名条 Esc 取消，不写入', async () => {
    mount()
    key('KeyK', { meta: true })
    fireEvent.click(await screen.findByTestId('clip-context-menu-item-rename'))
    const input = (await screen.findByTestId('clip-rename-input')) as HTMLInputElement
    fireEvent.change(input, { target: { value: 'x' } })
    key('Escape', { target: input })
    await waitFor(() => expect(screen.queryByTestId('clip-rename-input')).toBeNull())
    expect(ipc.called('clipboard_set_name')).toEqual([])
    // 焦点回到搜索框
    expect(document.activeElement).toBe(search())
  })
})

describe('芯片（识别出的片段）', () => {
  it('光标停下来之后检视：芯片条出现（验证码在前）；点芯片粘贴片段，⌥ 点击只复制', async () => {
    ipc.results.clipboard_inspect = OTP
    mount([meta(1, { preview: '【Wisp】验证码 482913，客服 138 0013 8000' }), meta(2)])
    await settleInspect()
    expect(ipc.called('clipboard_inspect')).toEqual([{ id: 1 }])
    const strip = await screen.findByTestId('clip-chips')
    const chips = within(strip).getAllByTestId('clip-chip')
    expect(chips.map((c) => c.textContent)).toEqual(['482913', '13800138000'])
    fireEvent.click(chips[1] as HTMLElement, { altKey: true })
    expect(ipc.called('clipboard_paste_text')).toEqual([
      { text: '13800138000', from: 1, mode: 'copy_only' },
    ])
  })

  it('⌘E 进入芯片条：→ 移动、↩ 粘贴当前片段；Esc 回到列表；↑↓ 换条目时芯片条随之收起', async () => {
    ipc.results.clipboard_inspect = ({ id }: { id: number }) =>
      id === 1 ? OTP : { entities: [], transforms: [] }
    mount([meta(1, { preview: 'otp' }), meta(2), meta(3)])
    await settleInspect()
    await screen.findByTestId('clip-chips')
    key('KeyE', { meta: true })
    expect(screen.getAllByTestId('clip-chip')[0]?.hasAttribute('data-active')).toBe(true)
    expect(search().getAttribute('aria-activedescendant')).toBe('clip-chip-0')
    key('ArrowRight')
    expect(screen.getAllByTestId('clip-chip')[1]?.hasAttribute('data-active')).toBe(true)
    key('Enter')
    expect(ipc.called('clipboard_paste_text')).toEqual([
      { text: '13800138000', from: 1, mode: 'normal' },
    ])
    expect(ipc.called('clipboard_paste')).toEqual([])
    key('KeyE', { meta: true })
    key('Escape')
    expect(screen.getAllByTestId('clip-chip')[0]?.hasAttribute('data-active')).toBe(false)
    expect(search().getAttribute('aria-activedescendant')).toBe('clip-row-1')
    key('ArrowDown')
    await waitFor(() => expect(screen.queryByTestId('clip-chips')).toBeNull())
  })

  it('识别设置变了（settings:changed）：丢掉按旧设置算出的检视结果，当前条目重新检视', async () => {
    const card: ClipInspect = {
      entities: [
        { kind: 'bank_card', value: '4111111111111111', start: 0, end: 19, sensitive: true },
      ],
      transforms: [],
    }
    ipc.results.clipboard_inspect = card
    mount([meta(1, { preview: '4111 1111 1111 1111' }), meta(2)])
    await settleInspect()
    await screen.findByTestId('clip-chips')
    // 常驻的悬浮窗里关掉「证件与银行卡号」：Rust 按新设置不再给出卡号
    ipc.results.clipboard_inspect = { entities: [], transforms: [] }
    const smart = {
      contacts: true,
      codes: true,
      numbers: true,
      identity: false,
      otp_auto_delete: true,
    }
    act(() =>
      ipc.emit('settings:changed', {
        settings: { clipboard: { smart } },
      } as unknown as SettingsSnapshot),
    )
    await waitFor(() => expect(screen.queryByTestId('clip-chips')).toBeNull())
    await settleInspect()
    expect(ipc.called('clipboard_inspect')).toEqual([{ id: 1 }, { id: 1 }])
    expect(screen.queryByTestId('clip-chips')).toBeNull()
    // 识别设置没变的 settings:changed（改的是别的设置）不重新检视
    act(() =>
      ipc.emit('settings:changed', {
        settings: { clipboard: { smart: { ...smart } } },
      } as unknown as SettingsSnapshot),
    )
    await settleInspect()
    expect(ipc.called('clipboard_inspect')).toHaveLength(2)
  })

  it('图片 / 文件不检视', async () => {
    mount([meta(1, { kind: 'image', preview: '' }), meta(2)])
    await settleInspect()
    expect(ipc.called('clipboard_inspect')).toEqual([])
  })

  it('检视结果缺失（替身没实现 clipboard_inspect）：当作没有片段，芯片条不出现、⌘E 不进入，也不算失败', async () => {
    // 没有登记的命令返回 undefined：落 store 时若直接读字段会在成功回调里抛错，变成没人处理的拒绝
    delete ipc.results.clipboard_inspect
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    mount([meta(1, { preview: 'otp' }), meta(2)])
    await settleInspect()
    expect(ipc.called('clipboard_inspect')).toEqual([{ id: 1 }])
    expect(screen.queryByTestId('clip-chips')).toBeNull()
    key('KeyE', { meta: true })
    expect(screen.queryByTestId('clip-chip')).toBeNull()
    expect(search().getAttribute('aria-activedescendant')).toBe('clip-row-1')
    expect(warn).not.toHaveBeenCalled()
  })
})

describe('粘贴为…', () => {
  it('⌥⇧↩ 直接打开到「粘贴为」子页；选一种变换按它粘贴（变换在 Rust 里做）', async () => {
    ipc.results.clipboard_inspect = OTP
    mount([meta(1, { preview: ' Hello ' }), meta(2)])
    await settleInspect()
    key('Enter', { alt: true, shift: true })
    const page = await screen.findByTestId('clip-menu-paste_as')
    const items = within(page).getAllByRole('menuitem')
    // 返回行 + 两种变换
    expect(items.map((i) => i.textContent)).toEqual([
      t('clipboard.action.paste_as'),
      t('smart.transform.trim'),
      t('smart.transform.upper'),
    ])
    fireEvent.click(screen.getByTestId('clip-menu-paste_as-item-transform-upper'))
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 1, mode: 'normal', transform: 'upper' }])
  })

  it('右键菜单里的「粘贴为」带「›」，进入子页再返回', async () => {
    ipc.results.clipboard_inspect = OTP
    mount([meta(1, { preview: 'otp' }), meta(2)])
    await settleInspect()
    key('KeyK', { meta: true })
    const item = await screen.findByTestId('clip-context-menu-item-paste_as')
    expect(item.getAttribute('aria-haspopup')).toBe('menu')
    fireEvent.click(item)
    await screen.findByTestId('clip-menu-paste_as')
    fireEvent.click(screen.getByTestId('clip-menu-paste_as-item-back'))
    expect(await screen.findByTestId('clip-context-menu')).toBeTruthy()
    // 「提取」只在有片段时列出，同样是子页
    fireEvent.click(screen.getByTestId('clip-context-menu-item-extract'))
    const extract = await screen.findByTestId('clip-menu-extract')
    fireEvent.click(within(extract).getByTestId('clip-menu-extract-item-entity-0'))
    expect(ipc.called('clipboard_paste_text')).toEqual([
      { text: '482913', from: 1, mode: 'normal' },
    ])
  })
})

describe('多选', () => {
  it('⌘ 点击按选择顺序编号；提示条换成多选的一组；↩ 合并粘贴（选择顺序）', () => {
    mount()
    fireEvent.click(row(4), { metaKey: true })
    fireEvent.click(row(3), { metaKey: true })
    expect(row(4).querySelector('[data-testid="clip-mark"]')?.textContent).toBe('1')
    expect(row(3).querySelector('[data-testid="clip-mark"]')?.textContent).toBe('2')
    expect(row(3).getAttribute('aria-label')).toContain(t('clipboard.a11y.marked', { n: 2 }))
    const hints = screen.getByTestId('clip-hints')
    expect(hints.textContent).toContain(t('clipboard.hint.paste_merged'))
    expect(hints.textContent).toContain(t('clipboard.hint.clear_marks'))
    key('Enter')
    expect(ipc.called('clipboard_paste_many')).toEqual([{ ids: [4, 3], mode: 'normal' }])
    expect(ipc.called('clipboard_paste')).toEqual([])
  })

  it('已有选择时单击也是切换（不粘贴）；Esc 先取消选择，面板不关', () => {
    const { props } = mount()
    fireEvent.click(row(3), { metaKey: true })
    fireEvent.click(row(4))
    expect(ipc.called('clipboard_paste')).toEqual([])
    expect(document.querySelectorAll('[data-testid="clip-mark"]')).toHaveLength(2)
    key('Escape')
    expect(document.querySelectorAll('[data-testid="clip-mark"]')).toHaveLength(0)
    expect(props.requestHide).not.toHaveBeenCalled()
  })

  it('⇧↓ 从光标扩展选择，⇧↑ 收回', () => {
    mount()
    key('ArrowDown')
    key('ArrowDown') // 光标在 3
    key('ArrowDown', { shift: true })
    key('ArrowDown', { shift: true })
    const marked = () =>
      [...document.querySelectorAll('[data-testid="clip-mark"]')].map((m) =>
        Number(m.closest('[data-clip-id]')?.getAttribute('data-clip-id')),
      )
    expect(marked().sort()).toEqual([3, 4, 5])
    key('ArrowUp', { shift: true })
    expect(marked().sort()).toEqual([3, 4])
  })

  it('⌘D 收藏全部；⌫ 删除含收藏的选择时先确认，再按一次删除全部', async () => {
    ipc.results.clipboard_delete = undefined
    mount()
    fireEvent.click(row(3), { metaKey: true })
    fireEvent.click(row(4), { metaKey: true })
    key('KeyD', { meta: true })
    await waitFor(() =>
      expect(ipc.called('clipboard_pin')).toEqual([
        { id: 3, pinned: true },
        { id: 4, pinned: true },
      ]),
    )
    key('Backspace')
    // 说明里的按键是 Kbd 节点：核对它后面的那段文字
    const tail = t('clipboard.confirm.delete_many', { n: 2, key: '|' }).split('|')[1] ?? ''
    expect(await screen.findByTestId('clip-confirm-delete')).toHaveTextContent(tail.trim())
    expect(ipc.called('clipboard_delete')).toEqual([])
    key('Backspace')
    await waitFor(() =>
      expect(ipc.called('clipboard_delete')).toEqual([
        { id: 3, force: true },
        { id: 4, force: true },
      ]),
    )
    expect(document.querySelectorAll('[data-testid="clip-mark"]')).toHaveLength(0)
  })

  it('右键一个没有被选择的行：菜单只作用于那一行', async () => {
    mount()
    fireEvent.click(row(3), { metaKey: true })
    fireEvent.click(row(4), { metaKey: true })
    fireEvent.contextMenu(row(6), { clientX: 10, clientY: 10 })
    const menu = await screen.findByTestId('clip-context-menu')
    const label = (id: string) =>
      within(menu).getByTestId(`clip-context-menu-item-${id}`).querySelector('.ui-menu-label-text')
        ?.textContent
    expect(label('paste')).toBe(t('clipboard.action.paste'))
    expect(within(menu).queryByTestId('clip-context-menu-item-rename')).not.toBeNull()
    fireEvent.click(within(menu).getByTestId('clip-context-menu-item-paste'))
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 6, mode: 'normal' }])
  })

  it('右键选择里的行：菜单作用于整个选择（合并粘贴；只作用于一条的操作不列出）', async () => {
    mount()
    fireEvent.click(row(3), { metaKey: true })
    fireEvent.click(row(4), { metaKey: true })
    fireEvent.contextMenu(row(4), { clientX: 10, clientY: 10 })
    const menu = await screen.findByTestId('clip-context-menu')
    expect(
      within(menu).getByTestId('clip-context-menu-item-paste').querySelector('.ui-menu-label-text')
        ?.textContent,
    ).toBe(t('clipboard.action.paste_merged'))
    expect(within(menu).queryByTestId('clip-context-menu-item-rename')).toBeNull()
    expect(within(menu).queryByTestId('clip-context-menu-item-preview')).toBeNull()
    fireEvent.click(within(menu).getByTestId('clip-context-menu-item-paste'))
    expect(ipc.called('clipboard_paste_many')).toEqual([{ ids: [3, 4], mode: 'normal' }])
  })
})

describe('其他', () => {
  it('验证码条目：行尾「验证码 · N 分钟后删除」（按分钟向上取整）；收藏之后不再显示', () => {
    // 视图的时间基准是打开时的当前时间
    const now = Date.now()
    mount([
      meta(1, { expires_at: now + 9.5 * 60_000 }),
      meta(2, { expires_at: now + 20_000 }),
      meta(3, { expires_at: now + 5 * 60_000, pinned: true }),
    ])
    expect(row(1).querySelector('[data-testid="clip-otp"]')?.textContent).toBe(
      t('clipboard.meta.otp_expires', { n: 10 }),
    )
    expect(row(2).querySelector('[data-testid="clip-otp"]')?.textContent).toBe(
      t('clipboard.meta.otp_soon'),
    )
    expect(row(3).querySelector('[data-testid="clip-otp"]')).toBeNull()
  })

  it('新条目到达时按当前时间刷新：开了很久之后新验证码的倒计时不按陈旧的时间基准', () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] })
    try {
      const start = Date.now()
      // 行都较旧：下一次刷新排在很久之后
      mount([
        meta(1, { last_used_at: start - 3 * HOUR }),
        meta(2, { last_used_at: start - 5 * HOUR }),
      ])
      // 半小时过去，定时器还没到点
      vi.setSystemTime(start + 30 * 60_000)
      const now = Date.now()
      ipc.changed({
        change: 'inserted',
        item: meta(9, { created_at: now, last_used_at: now, expires_at: now + 10 * 60_000 }),
      })
      expect(row(9).querySelector('[data-testid="clip-otp"]')?.textContent).toBe(
        t('clipboard.meta.otp_expires', { n: 10 }),
      )
    } finally {
      vi.useRealTimers()
    }
  })

  it('视图挂上之前到达的改动（悬浮窗 ready 之后、attach 之前）：挂上时重新拉取首页，不漏掉', async () => {
    const fresh = meta(30, { last_used_at: NOW + 1, preview: 'arrived early' })
    ipc.results.clipboard_recent = () => ({
      items: [fresh, ...sample()].map((item) => ({ item, highlights: [] })),
      pinned_count: 0,
      next_offset: null,
      query: null,
      filter: 'all',
    })
    installClipboardEvents()
    awaitClipboardView()
    ipc.changed({ change: 'inserted', item: fresh })
    mount()
    await waitFor(() => expect(rowIds()).toContain(30))
    expect(ipc.called('clipboard_recent')).toHaveLength(1)
  })

  it('图片的「识别文字」：clipboard_send_to(ocr)', async () => {
    ipc.results.clipboard_send_to = undefined
    mount([meta(1, { kind: 'image', preview: '' }), meta(2)])
    key('KeyK', { meta: true })
    fireEvent.click(await screen.findByTestId('clip-context-menu-item-ocr'))
    expect(ipc.called('clipboard_send_to')).toEqual([{ id: 1, target: 'ocr' }])
  })

  it('⌘P / 头部的钉子 / ⋯ 菜单：交接到悬浮剪贴板，带上当前的查询、筛选与光标', async () => {
    mount()
    expect(key('KeyP', { meta: true }).defaultPrevented).toBe(true)
    expect(ipc.called('clipboard_float_open')).toEqual([
      { query: '', filter: 'all', selected_id: expect.any(Number) },
    ])
    fireEvent.click(screen.getByTestId('clip-tool-float_open'))
    expect(ipc.called('clipboard_float_open')).toHaveLength(2)
    fireEvent.click(screen.getByTestId('clip-more'))
    await screen.findByTestId('clip-more-menu')
    expect(screen.getByTestId('clip-more-menu-item-float_open')).toBeTruthy()
  })
})
