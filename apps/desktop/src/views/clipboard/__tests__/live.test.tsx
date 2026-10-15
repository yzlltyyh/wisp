/**
 * 实时变化与防误点（其他设备同步来的条目在面板打开时到达）：
 * - `reloaded`：Rust 在显示期间改发逐条事件；收到 `reloaded` 时立即重新拉首页（不留下过时的行），只有退出动画期间
 *   暂存到隐藏之后或下一次打开；
 * - 清空确认：同步剪贴板历史时注明其他设备上的记录也会一并清除（打开面板时不读设置，打开菜单 / 发起清空时才读）；
 * - 行的位置刚因为数据变化改变过（不到 --dur-base，或按下指针之后才变）：单击只选中、不粘贴；
 * - updated 带来了新的排序键（另一台设备改了置顶）：挪到新位置。
 */
import { act, fireEvent, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { t } from '../../../shared/i18n/i18n'
import { setTransport } from '../../../shared/ipc'
import type { ClipItemMeta, ClipPage } from '../../../shared/ipc/types'
import type { ViewProps } from '../../../panel/types'
import { setWindowHidden } from '../../../shared/hooks/useHidden'
import { duration, ms } from '../../../shared/motion/tokens'
import { ClipboardView } from '../ClipboardView'
import { applyChange, initialState, visibleHits } from '../store'
import { clipVisibility, resetWebviewClipboardForTest } from '../useClipboard'
import {
  fakeIpc,
  hit,
  idleTransport,
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
import { MotionRoot } from '../../../shared/motion/MotionRoot'

let ipc: FakeIpc
/** performance.now 的偏移：把「过了多久」交给用例决定（防误点的窗口按它计时） */
let skew = 0

function mount(extra: Partial<ViewProps<'clipboard'>> = {}) {
  const p = viewProps<'clipboard'>(payload(sample()), extra)
  const r = renderIn(<ClipboardView {...p} />)
  const rerender = (next: Partial<typeof p>) =>
    r.rerender(
      <MotionRoot>
        <ClipboardView {...p} {...next} />
      </MotionRoot>,
    )
  return { ...r, rerender }
}

const row = (id: number) => document.getElementById(`clip-row-${id}`) as HTMLElement
const selected = () =>
  (screen.getByTestId('clip-search') as HTMLInputElement).getAttribute('aria-activedescendant')
const rowIds = () =>
  screen.getAllByTestId('clip-row').map((r) => Number(r.getAttribute('data-clip-id')))
const later = (msec: number) => {
  skew += msec
}

function page(items: readonly ClipItemMeta[]): ClipPage {
  return {
    items: items.map((item) => hit(item)),
    pinned_count: items.filter((item) => item.pinned).length,
    next_offset: null,
    query: null,
    filter: 'all',
  }
}

beforeEach(() => {
  ipc = fakeIpc()
  ipc.results.clipboard_paste = { type: 'pasted' }
  ipc.results.clipboard_recent = page([
    meta(50, { last_used_at: NOW + 5, preview: 'synced' }),
    ...sample(),
  ])
  setPlatform('macos')
  skew = 0
  const real = performance.now.bind(performance)
  vi.spyOn(performance, 'now').mockImplementation(() => real() + skew)
})

afterEach(() => {
  act(() => setWindowHidden(false))
  vi.restoreAllMocks()
  restorePlatform()
  setTransport(idleTransport())
  resetWebviewClipboardForTest()
})

describe('reloaded', () => {
  it('显示中收到也立即重新拉首页：不把过时的行留到下次打开；选中项还在时保持选中', async () => {
    ipc.results.clipboard_recent = page([meta(50, { last_used_at: NOW + 5, preview: 'synced' })])
    mount()
    const before = selected()
    ipc.changed({ change: 'reloaded' })
    expect(ipc.called('clipboard_recent')).toEqual([{ offset: 0, limit: 200, filter: 'all' }])
    await act(async () => {})
    // 别处清空后只剩一条：被删掉的行立即消失，而不是留在眼前、点了才说「这条记录已不存在」
    expect(rowIds()).toEqual([50])
    expect(selected()).not.toBe(before)
    expect(selected()).toBe('clip-row-50')
    // 之后隐藏、再打开都不再重复拉取
    act(() => setWindowHidden(true))
    act(() => setWindowHidden(false))
    expect(ipc.called('clipboard_recent')).toHaveLength(1)
  })

  it('还在打开的过程中（准备 / 进入动画）就立即重新拉首页', async () => {
    mount({ phase: 'entering' })
    ipc.changed({ change: 'reloaded' })
    expect(ipc.called('clipboard_recent')).toHaveLength(1)
    await act(async () => {})
    expect(rowIds()).toContain(50)
  })

  it('退出动画期间收到的暂存，下一次打开时处理（多次只拉一次）', () => {
    const v = mount()
    v.rerender({ phase: 'exiting' })
    ipc.changed({ change: 'reloaded' })
    ipc.changed({ change: 'reloaded' })
    expect(ipc.called('clipboard_recent')).toEqual([])
    v.rerender({ phase: 'preparing' })
    expect(ipc.called('clipboard_recent')).toHaveLength(1)
    // 只拉一次；之后显示中不再拉
    v.rerender({ phase: 'shown' })
    expect(ipc.called('clipboard_recent')).toHaveLength(1)
  })

  it('可见程度：窗口隐藏优先，其次按面板阶段', () => {
    expect(clipVisibility('shown', true)).toBe('hidden')
    expect(clipVisibility('hidden', false)).toBe('hidden')
    expect(clipVisibility('shown', false)).toBe('shown')
    expect(clipVisibility('exiting', false)).toBe('leaving')
    expect(clipVisibility('preparing', false)).toBe('opening')
    expect(clipVisibility('entering', false)).toBe('opening')
  })
})

describe('清空确认', () => {
  const syncSettings = (enabled: boolean, clipboard = true) => ({
    settings: { sync: { enabled, content: { clipboard } } },
  })
  const openClear = async () => {
    act(() => screen.getByTestId('clip-more').click())
    await act(async () => {})
    act(() => screen.getByTestId('clip-more-menu-item-clear').click())
    return screen.findByTestId('clip-confirm-clear')
  }

  it('打开面板时不读设置；打开菜单时读一次，同步剪贴板历史时注明其他设备也会清除', async () => {
    ipc.results.settings_get = syncSettings(true)
    mount()
    expect(ipc.called('settings_get')).toEqual([])
    const confirm = await openClear()
    expect(ipc.called('settings_get').length).toBeGreaterThan(0)
    await waitFor(() => expect(confirm).toHaveTextContent(t('clipboard.confirm.clear_synced')))
  })

  it('没有同步剪贴板历史时只问本机', async () => {
    ipc.results.settings_get = syncSettings(true, false)
    mount()
    const confirm = await openClear()
    await act(async () => {})
    expect(confirm).toHaveTextContent(t('clipboard.confirm.clear'))
    expect(confirm).not.toHaveTextContent(t('clipboard.confirm.clear_synced'))
  })

  it('读不到设置：确认照常出现（不注明），清空照常进行', async () => {
    ipc.results.settings_get = () => {
      throw { code: 'internal', message: 'x', llm: null }
    }
    ipc.results.clipboard_clear = 0
    mount()
    const confirm = await openClear()
    expect(confirm).toHaveTextContent(t('clipboard.confirm.clear'))
    act(() =>
      within(confirm)
        .getByRole('button', { name: t('clipboard.confirm.clear_action') })
        .click(),
    )
    await waitFor(() => expect(ipc.called('clipboard_clear')).toHaveLength(1))
  })
})

describe('防误点', () => {
  const insertAbove = () =>
    ipc.changed({
      change: 'inserted',
      item: meta(90, { last_used_at: NOW + 1_000, preview: 'from another device' }),
    })

  it('行刚被上面插入的条目挤下去：单击只选中，过了 --dur-base 再单击才粘贴', () => {
    mount()
    insertAbove()
    expect(rowIds().slice(0, 4)).toEqual([1, 2, 90, 3])
    fireEvent.click(row(4))
    expect(ipc.called('clipboard_paste')).toEqual([])
    expect(selected()).toBe('clip-row-4')
    later(ms(duration.base) + 10)
    fireEvent.click(row(4))
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 4, mode: 'normal' }])
  })

  it('按下指针之后行才换了位置：这次单击不粘贴', () => {
    mount()
    fireEvent.pointerDown(row(6), { button: 0 })
    insertAbove()
    later(ms(duration.base) * 4)
    fireEvent.click(row(6))
    expect(ipc.called('clipboard_paste')).toEqual([])
    expect(selected()).toBe('clip-row-6')
    // 下一次完整的按下 → 松开照常粘贴
    fireEvent.pointerDown(row(6), { button: 0 })
    fireEvent.click(row(6))
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 6, mode: 'normal' }])
  })

  it('位置没变的行不受影响：别处的变化（缩略图更新）之后照常单击粘贴', () => {
    mount()
    ipc.changed({ change: 'updated', item: { ...sample()[4]!, preview: 'with thumb' } })
    fireEvent.pointerDown(row(7), { button: 0 })
    fireEvent.click(row(7))
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 7, mode: 'normal' }])
  })

  it('删除让下面的行合拢：合拢的行同样防误点', () => {
    mount()
    ipc.changed({ change: 'removed', id: 3 })
    fireEvent.click(row(4))
    expect(ipc.called('clipboard_paste')).toEqual([])
    later(ms(duration.base) + 1)
    fireEvent.click(row(4))
    expect(ipc.called('clipboard_paste')).toEqual([{ id: 4, mode: 'normal' }])
  })
})

describe('updated 带来新的排序键', () => {
  it('另一台设备置顶了一条：挪进置顶组，而不是原地改标记', () => {
    const s = initialState(payload(sample()), NOW)
    const item = { ...sample()[5]!, pinned: true }
    const next = { ...s, ...applyChange(s, { change: 'updated', item }) }
    expect(visibleHits(next).map((h) => h.item.id)).toEqual([6, 1, 2, 3, 4, 5, 7, 8])
    expect(visibleHits(next)[0]?.item.pinned).toBe(true)
  })

  it('排序键没变：原地替换；不在列表里的条目不会被加进来', () => {
    const s = initialState(payload(sample()), NOW)
    const next = {
      ...s,
      ...applyChange(s, { change: 'updated', item: { ...sample()[2]!, preview: 'x' } }),
    }
    expect(visibleHits(next).map((h) => h.item.id)).toEqual([1, 2, 3, 4, 5, 6, 7, 8])
    expect(visibleHits(next)[2]?.item.preview).toBe('x')
    const outside = {
      ...s,
      ...applyChange(s, { change: 'updated', item: meta(77, { pinned: true }) }),
    }
    expect(visibleHits(outside).map((h) => h.item.id)).toEqual([1, 2, 3, 4, 5, 6, 7, 8])
  })
})
