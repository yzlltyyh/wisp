/**
 * 设置窗口的识别历史与排除名单：
 * - 识别历史：列表、深链锚点、`ocr_history:changed` 原地增删、复制 / 重新翻译 / 删除、
 *   空状态、关开关时询问「清空 N 条」；
 * - 排除名单的应用选择器：最近来源与正在运行的应用合并（最近来源在前、按 id 去重）、读取失败退回、过滤。
 */
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { getAppearance, setAppearance } from '../../shared/hooks/useAppearance'
import { boot } from '../../shared/ipc'
import type { OcrHistoryItem, SourceApp, WindowTarget } from '../../shared/ipc/types'
import { resetAdviceCache } from '../components/AdviceBanners'
import { resetHotkeyTest } from '../components/hotkeyTest'
import { resetStyleSaves } from '../lib/styleSave'
import { resetWrites } from '../lib/writes'
import { applyChange, PAGE } from '../pages/OcrHistory'
import { mergeAppCandidates } from '../pages/ClipboardPage'
import { resetTestRuns } from '../providers/hooks'
import { resetTemplatesCache } from '../providers/templates'
import { SettingsApp } from '../SettingsApp'
import { resetSettingsStore, setQueueDelay } from '../store'
import { installHarness, snapshot } from './harness'

const appearance = getAppearance()

function item(id: number, text = `识别结果 ${id}`): OcrHistoryItem {
  return {
    id,
    created_at: Date.now() - id * 60_000,
    mode: 'lines',
    model: 'qwen3-vl-plus',
    text,
    thumb_url: `ocrimg/t${id}`,
    image_url: id % 2 ? `ocrimg/i${id}` : null,
  }
}

function app(id: string, name: string): SourceApp {
  return { id, name, icon_url: null }
}

function renderAt(route: WindowTarget | null) {
  boot.route = route
  return render(<SettingsApp />)
}

beforeEach(() => {
  resetSettingsStore()
  resetTemplatesCache()
  resetTestRuns()
  resetAdviceCache()
  resetHotkeyTest()
  resetStyleSaves()
  resetWrites()
  setQueueDelay(null)
  setAppearance({ ...appearance, os: 'macos', uiLanguage: 'zh-CN' })
  boot.os = 'macos'
})
afterEach(() => {
  boot.route = null
  vi.useRealTimers()
})

describe('识别历史：纯函数', () => {
  it('inserted 插在最前、按 id 去重；removed / cleared 原地生效', () => {
    const list = [item(2), item(1)]
    expect(applyChange(list, { change: 'inserted', item: item(3) }).map((x) => x.id)).toEqual([
      3, 2, 1,
    ])
    expect(applyChange(list, { change: 'inserted', item: item(2) }).map((x) => x.id)).toEqual([
      2, 1,
    ])
    expect(applyChange(list, { change: 'removed', ids: [1] }).map((x) => x.id)).toEqual([2])
    expect(applyChange(list, { change: 'cleared' })).toEqual([])
  })
})

describe('识别历史（设置 › OCR）', () => {
  function historyHarness(items: OcrHistoryItem[], on = true) {
    const snap = snapshot()
    snap.settings.ocr.history = on
    const h = installHarness(snap)
    let list = items
    h.handle('ocr_history_list', ({ offset, limit }) =>
      list.slice(offset as number, (offset as number) + (limit as number)),
    )
    h.handle('ocr_history_delete', ({ id }) => {
      list = list.filter((x) => x.id !== id)
      h.emit('ocr_history:changed', { change: 'removed', ids: [id as number] })
    })
    h.handle('ocr_history_clear', () => {
      list = []
      h.emit('ocr_history:changed', { change: 'cleared' })
    })
    return h
  }

  it('深链锚点 ocr_history：打开 OCR 分组、滚到识别历史并聚焦分组标题；列表显示时间 · 模式 · 模型', async () => {
    const scrolled = vi.fn<(arg?: unknown) => void>()
    Element.prototype.scrollIntoView = scrolled
    historyHarness([item(1, '第一条\n第二行'), item(2)])
    renderAt({ kind: 'settings', section: 'ocr', anchor: 'ocr_history' })
    const list = await screen.findByTestId('ocr-history-list')
    expect(within(list).getAllByRole('listitem')).toHaveLength(2)
    expect(screen.getByTestId('ocr-history-item-1')).toHaveTextContent('第一条')
    expect(screen.getByTestId('ocr-history-item-1')).toHaveTextContent('纯文本 · qwen3-vl-plus')
    await waitFor(() => expect(scrolled).toHaveBeenCalled())
    const section = screen.getByTestId('ocr-history-section')
    expect(section.querySelector('.ui-section-title')).toHaveFocus()
    expect(screen.getByTestId('ocr-history-count')).toHaveTextContent('2 条记录')
  })

  it('已经停在 OCR 页时再次深链：重新高亮（data-flash 先去掉再设置），按动画时长移除', async () => {
    Element.prototype.scrollIntoView = vi.fn<(arg?: unknown) => void>()
    setAppearance({ ...appearance, os: 'macos', uiLanguage: 'zh-CN', reduceMotion: false })
    const h = historyHarness([item(1)])
    renderAt({ kind: 'settings', section: 'ocr', anchor: 'ocr_history' })
    await screen.findByTestId('ocr-history-list')
    const section = screen.getByTestId('ocr-history-section')
    await waitFor(() => expect(section).toHaveAttribute('data-flash'))
    // 高亮按时长移除（不依赖 animationend；consumeFocus 之后 effect 的清理不会把它留下）
    await waitFor(() => expect(section).not.toHaveAttribute('data-flash'), { timeout: 3000 })
    act(() =>
      h.emit('window:navigate', { kind: 'settings', section: 'ocr', anchor: 'ocr_history' }),
    )
    await waitFor(() => expect(section).toHaveAttribute('data-flash'))
  })

  it('键盘：只有当前行在 Tab 序列里（roving tabindex），↑↓ 在行之间移动；确认条不重复播报（没有 role=alert）', async () => {
    historyHarness([item(1), item(2), item(3)])
    renderAt({ kind: 'settings', section: 'ocr' })
    await screen.findByTestId('ocr-history-list')
    const main = (id: number) =>
      within(screen.getByTestId(`ocr-history-item-${id}`)).getByRole('button', { expanded: false })
    expect(main(1)).not.toHaveAttribute('tabindex')
    expect(screen.getByTestId('ocr-history-delete-1')).not.toHaveAttribute('tabindex')
    expect(main(2)).toHaveAttribute('tabindex', '-1')
    expect(screen.getByTestId('ocr-history-copy-2')).toHaveAttribute('tabindex', '-1')
    main(1).focus()
    await userEvent.keyboard('{ArrowDown}')
    expect(main(2)).toHaveFocus()
    await waitFor(() => expect(main(1)).toHaveAttribute('tabindex', '-1'))
    expect(main(2)).not.toHaveAttribute('tabindex')
    await userEvent.keyboard('{End}')
    expect(main(3)).toHaveFocus()
    await userEvent.keyboard('{Home}')
    expect(main(1)).toHaveFocus()
    // 关掉开关：询问条打开时焦点在「保留」，不再用 role=alert 重复播报
    await userEvent.click(screen.getByTestId('ocr-history'))
    const prompt = await screen.findByTestId('ocr-history-off-prompt')
    expect(prompt).not.toHaveAttribute('role')
  })

  it('点按一行展开回看（原图 + 全文，aria-expanded）；复制 / 重新翻译（trigger_action origin = ocr_history）/ 删除', async () => {
    const h = historyHarness([item(1, '全文 A'), item(2)])
    renderAt({ kind: 'settings', section: 'ocr' })
    await screen.findByTestId('ocr-history-list')
    const row = screen.getByTestId('ocr-history-item-1')
    const toggle = within(row).getByRole('button', { expanded: false })
    await userEvent.click(toggle)
    expect(toggle).toHaveAttribute('aria-expanded', 'true')
    expect(await screen.findByTestId('ocr-history-text-1')).toHaveTextContent('全文 A')
    // 回看的截图：按缩略图的宽高比占位、先放大显示缩略图，原图在后台解码进 canvas 后交叉淡入
    expect(within(row).getByRole('img', { name: '识别时的截图' })).toHaveAttribute(
      'src',
      'ocrimg/t1',
    )
    const shot = screen.getByTestId('ocr-history-image-1')
    expect(shot.querySelector('canvas')).not.toBeNull()
    expect(shot.style.aspectRatio).not.toBe('')

    await userEvent.click(screen.getByTestId('ocr-history-copy-1'))
    await waitFor(() => expect(h.callsOf('clipboard_copy_text')).toEqual([{ text: '全文 A' }]))

    await userEvent.click(screen.getByTestId('ocr-history-translate-1'))
    await waitFor(() =>
      expect(h.callsOf('trigger_action')).toEqual([
        { action: 'translate_selection', text: '全文 A', origin: 'ocr_history' },
      ]),
    )

    await userEvent.click(screen.getByTestId('ocr-history-delete-2'))
    await waitFor(() => expect(h.callsOf('ocr_history_delete')).toEqual([{ id: 2 }]))
    await waitFor(() => expect(screen.queryByTestId('ocr-history-item-2')).toBeNull())
  })

  it('截图翻译的条目（mode = translate，存的是译文）没有「重新翻译」，复制照常；其余模式都有', async () => {
    const translated: OcrHistoryItem = { ...item(1, 'Translated text'), mode: 'translate' }
    const others: OcrHistoryItem[] = (['lines', 'paragraphs', 'markdown'] as const).map(
      (mode, i) => ({ ...item(i + 2), mode }),
    )
    const h = historyHarness([translated, ...others])
    renderAt({ kind: 'settings', section: 'ocr' })
    await screen.findByTestId('ocr-history-list')
    expect(screen.queryByTestId('ocr-history-translate-1')).toBeNull()
    for (const x of others) {
      expect(screen.getByTestId(`ocr-history-translate-${x.id}`)).toBeInTheDocument()
    }
    // 「重新翻译」在最左：缺了它的行里「复制」「删除」仍是最后两个按钮（与其他行上下对齐）
    const actions = (id: number) =>
      [
        ...(screen
          .getByTestId(`ocr-history-item-${id}`)
          .querySelectorAll<HTMLElement>('.st-ocrh-actions button') ?? []),
      ].map((b) => b.dataset.testid)
    expect(actions(1).slice(-2)).toEqual(['ocr-history-copy-1', 'ocr-history-delete-1'])
    expect(actions(2)).toEqual([
      'ocr-history-translate-2',
      'ocr-history-copy-2',
      'ocr-history-delete-2',
    ])
    await userEvent.click(screen.getByTestId('ocr-history-copy-1'))
    await waitFor(() =>
      expect(h.callsOf('clipboard_copy_text')).toEqual([{ text: 'Translated text' }]),
    )
  })

  it('ocr_history:changed 原地增删（不整页重拉）', async () => {
    const h = historyHarness([item(1)])
    renderAt({ kind: 'settings', section: 'ocr' })
    await screen.findByTestId('ocr-history-list')
    act(() => h.emit('ocr_history:changed', { change: 'inserted', item: item(9, '新的一条') }))
    expect(await screen.findByTestId('ocr-history-item-9')).toHaveTextContent('新的一条')
    const ids = screen.getAllByRole('listitem').map((el) => el.getAttribute('data-testid'))
    expect(ids[0]).toBe('ocr-history-item-9')
    act(() => h.emit('ocr_history:changed', { change: 'removed', ids: [1] }))
    await waitFor(() => expect(screen.queryByTestId('ocr-history-item-1')).toBeNull())
    expect(h.callsOf('ocr_history_list')).toHaveLength(1)
  })

  it('删除失败：放回原位并提示', async () => {
    const h = historyHarness([item(1), item(2)])
    h.handle('ocr_history_delete', () => {
      throw { code: 'internal', message: 'db', llm: null }
    })
    renderAt({ kind: 'settings', section: 'ocr' })
    await screen.findByTestId('ocr-history-list')
    await userEvent.click(screen.getByTestId('ocr-history-delete-1'))
    await waitFor(() => expect(screen.getByTestId('ocr-history-item-1')).toBeInTheDocument())
    expect(await screen.findByTestId('settings-notice')).toBeInTheDocument()
  })

  it('「显示更多」读取失败：列表保留、按钮恢复可点，底部轻提示说明这次没读到', async () => {
    const h = historyHarness(Array.from({ length: PAGE + 5 }, (_, i) => item(i + 1)))
    h.handle('ocr_history_list', ({ offset, limit }) => {
      if ((offset as number) > 0) throw { code: 'internal', message: 'db', llm: null }
      return Array.from({ length: limit as number }, (_, i) => item(i + 1))
    })
    renderAt({ kind: 'settings', section: 'ocr' })
    await screen.findByTestId('ocr-history-list')
    await userEvent.click(screen.getByTestId('ocr-history-more'))
    expect(await screen.findByTestId('settings-notice')).toBeInTheDocument()
    expect(screen.getByTestId('ocr-history-list')).toBeInTheDocument()
    expect(screen.queryByTestId('ocr-history-failed')).toBeNull()
    await waitFor(() => expect(screen.getByTestId('ocr-history-more')).toBeEnabled())
  })

  it('空状态：关闭时说明开启后保存什么；开启但还没有记录时另一句', async () => {
    historyHarness([], false)
    const first = renderAt({ kind: 'settings', section: 'ocr' })
    expect(await screen.findByTestId('ocr-history-empty')).toHaveTextContent('识别历史已关闭')
    expect(screen.getByTestId('ocr-history-empty')).toHaveTextContent('最近 50 条')
    first.unmount()
    resetSettingsStore()
    historyHarness([], true)
    renderAt({ kind: 'settings', section: 'ocr' })
    expect(await screen.findByTestId('ocr-history-empty')).toHaveTextContent('还没有识别记录')
  })

  it('有记录时关掉开关：原地询问是否一并清空；「清空 N 条」清掉全部', async () => {
    const h = historyHarness([item(1), item(2)])
    renderAt({ kind: 'settings', section: 'ocr' })
    await screen.findByTestId('ocr-history-list')
    await userEvent.click(screen.getByTestId('ocr-history'))
    await waitFor(() =>
      expect(h.callsOf('settings_patch').at(-1)?.patch).toEqual({ ocr: { history: false } }),
    )
    const prompt = await screen.findByTestId('ocr-history-off-prompt')
    expect(prompt).toHaveTextContent('已保存的 2 条记录')
    // 焦点在安全的一侧（保留）
    expect(within(prompt).getByRole('button', { name: '保留' })).toHaveFocus()
    // 开关已经关了：危险按钮只说「清空 N 条」，不再说「关闭并清空」
    expect(screen.getByTestId('ocr-history-off-clear')).toHaveTextContent('清空 2 条')
    await userEvent.click(screen.getByTestId('ocr-history-off-clear'))
    await waitFor(() => expect(h.callsOf('ocr_history_clear')).toHaveLength(1))
    expect(await screen.findByTestId('ocr-history-empty')).toHaveTextContent('识别历史已关闭')
  })
})

describe('排除名单的应用选择器', () => {
  it('合并：最近来源在前、按 id 去重（不区分大小写）、去掉已排除的、按名称 / id 过滤', () => {
    const recent = [app('com.a', 'Alpha'), app('com.b', 'Beta')]
    const running = [app('COM.A', 'Alpha'), app('com.c', 'Gamma'), app('com.x', 'Excluded')]
    expect(mergeAppCandidates(recent, running, ['com.x'])).toEqual({
      recent,
      running: [app('com.c', 'Gamma')],
    })
    expect(mergeAppCandidates(recent, null, [])).toEqual({ recent, running: [] })
    expect(mergeAppCandidates(recent, running, [], 'gam').running).toEqual([app('com.c', 'Gamma')])
    expect(mergeAppCandidates(recent, running, [], 'com.b').recent).toEqual([app('com.b', 'Beta')])
  })

  it('打开时读取正在运行的应用并与最近来源合并；读取失败时只列最近来源', async () => {
    const h = installHarness()
    h.handle('clipboard_recent', () => ({
      items: [{ item: { source: app('com.recent', 'Recent') } }],
      total: 1,
    }))
    h.handle('apps_running', () => [app('com.recent', 'Recent'), app('com.run', 'Runner')])
    renderAt({ kind: 'settings', section: 'clipboard' })
    await userEvent.click(await screen.findByTestId('clip-excluded-add'))
    const picker = await screen.findByTestId('clip-app-picker')
    await within(picker).findByTestId('clip-app-option-com.run')
    expect(within(picker).getAllByTestId('clip-app-option-com.recent')).toHaveLength(1)
    // 占位行与真实行交叉淡出 / 淡入：淡出结束后只剩一个「正在运行」标题
    await waitFor(() => expect(within(picker).getAllByText('正在运行的应用')).toHaveLength(1))
    await userEvent.click(within(picker).getByTestId('clip-app-option-com.run'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    const patch = h.callsOf('settings_patch')[0]?.patch as {
      clipboard: { excluded_apps: Array<{ app_id: string }> }
    }
    expect(patch.clipboard.excluded_apps.some((a) => a.app_id === 'com.run')).toBe(true)
  })

  it('每次打开只读一次 apps_running（最近来源读回不会再读一遍）；过滤框从一开始就在、打开时焦点在它上面', async () => {
    const h = installHarness()
    let resolveRecent: (v: unknown) => void = () => undefined
    h.handle(
      'clipboard_recent',
      () =>
        new Promise((r) => {
          resolveRecent = r
        }),
    )
    h.handle('apps_running', () => [app('com.run', 'Runner')])
    renderAt({ kind: 'settings', section: 'clipboard' })
    await userEvent.click(await screen.findByTestId('clip-excluded-add'))
    const picker = await screen.findByTestId('clip-app-picker')
    // 过滤框一开始就在（不因候选晚到才插进来），焦点在它上面
    const filter = within(picker).getByTestId('clip-app-filter')
    await waitFor(() => expect(filter).toHaveFocus())
    // 读取期间「正在运行」是占位行
    expect(picker.querySelectorAll('.st-app-option-placeholder').length).toBeGreaterThan(0)
    await within(picker).findByTestId('clip-app-option-com.run')
    resolveRecent({ items: [{ item: { source: app('com.recent', 'Recent') } }], total: 1 })
    await within(picker).findByTestId('clip-app-option-com.recent')
    expect(h.callsOf('apps_running')).toHaveLength(1)
    expect(h.callsOf('clipboard_recent')).toHaveLength(1)
    // 关掉再打开：apps_running 再读一次，最近来源不再读
    await userEvent.keyboard('{Escape}')
    await userEvent.click(screen.getByTestId('clip-excluded-add'))
    await waitFor(() => expect(h.callsOf('apps_running')).toHaveLength(2))
    expect(h.callsOf('clipboard_recent')).toHaveLength(1)
  })

  it('键盘全流程：Enter 打开 → 焦点在过滤框 → ↓ 进入选项 → Enter 选中 → 焦点回到「添加」按钮', async () => {
    const h = installHarness()
    h.handle('clipboard_recent', () => ({ items: [], total: 0 }))
    h.handle('apps_running', () => [app('com.a', 'Alpha'), app('com.b', 'Beta')])
    renderAt({ kind: 'settings', section: 'clipboard' })
    const add = await screen.findByTestId('clip-excluded-add')
    add.focus()
    await userEvent.keyboard('{Enter}')
    const picker = await screen.findByTestId('clip-app-picker')
    await within(picker).findByTestId('clip-app-option-com.b')
    const filter = within(picker).getByTestId('clip-app-filter')
    await waitFor(() => expect(filter).toHaveFocus())
    await userEvent.keyboard('{ArrowDown}')
    expect(within(picker).getByTestId('clip-app-option-com.a')).toHaveFocus()
    await userEvent.keyboard('{ArrowDown}')
    expect(within(picker).getByTestId('clip-app-option-com.b')).toHaveFocus()
    // 第一个选项上 ↑ 回到过滤框
    await userEvent.keyboard('{ArrowUp}{ArrowUp}')
    expect(filter).toHaveFocus()
    await userEvent.keyboard('{ArrowDown}{ArrowDown}{Enter}')
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    const patch = h.callsOf('settings_patch')[0]?.patch as {
      clipboard: { excluded_apps: Array<{ app_id: string }> }
    }
    expect(patch.clipboard.excluded_apps.at(-1)?.app_id).toBe('com.b')
    await waitFor(() => expect(add).toHaveFocus())
    // 过滤框里 Enter：选中第一个候选
    await userEvent.keyboard('{Enter}')
    await within(await screen.findByTestId('clip-app-picker')).findByTestId('clip-app-option-com.a')
    await userEvent.keyboard('Alp{Enter}')
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(2))
    await waitFor(() => expect(add).toHaveFocus())
  })

  it('apps_running 失败：退回只列最近来源，不显示「正在运行」分组', async () => {
    const h = installHarness()
    h.handle('clipboard_recent', () => ({
      items: [{ item: { source: app('com.recent', 'Recent') } }],
      total: 1,
    }))
    h.handle('apps_running', () => {
      throw { code: 'platform', message: 'errors.ipc.platform', llm: null }
    })
    renderAt({ kind: 'settings', section: 'clipboard' })
    await userEvent.click(await screen.findByTestId('clip-excluded-add'))
    const picker = await screen.findByTestId('clip-app-picker')
    await within(picker).findByTestId('clip-app-option-com.recent')
    await waitFor(() => expect(within(picker).queryByText('加载中…')).toBeNull())
    expect(within(picker).queryByText('正在运行的应用')).toBeNull()
  })
})
