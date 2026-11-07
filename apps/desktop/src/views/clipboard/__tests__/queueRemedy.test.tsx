/**
 * 连续粘贴的补救入口（操作注册表的 queue_back / queue_skip）与悬浮窗里的说明：
 * - 悬浮窗头部有「退回一条」（点按不抢目标应用的焦点），面板头部没有；面板与悬浮窗的 ⋯ 菜单里有「退回一条」「跳过一条」；
 * - 只在队列进行中出现；已经在第一条之前时菜单里不列「退回一条」；
 * - 命令返回新的进度后在窗口里轻提示这一次的说明；退回返回 null（已经退到第一条之前）时说一声；命令失败时提示没做成；
 * - 悬浮剪贴板开着时 Rust 不弹 toast：`clipboard:queue` 带来的说明（留到下一次、粘贴键不对）由悬浮窗轻提示，不进 store。
 */
import { act, fireEvent, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport } from '../../../shared/ipc'
import { t } from '../../../shared/i18n/i18n'
import { ClipboardView } from '../ClipboardView'
import { ClipFloatContext, type ClipFloatShell } from '../host'
import { resetWebviewClipboardForTest } from '../useClipboard'
import {
  fakeIpc,
  idleTransport,
  payload,
  renderIn,
  restorePlatform,
  sample,
  setPlatform,
  viewProps,
  type FakeIpc,
} from './helpers'

let ipc: FakeIpc

function shell(): ClipFloatShell {
  return {
    seed: { query: '', filter: 'all', selectedId: null },
    onTop: true,
    setOnTop: vi.fn<(on: boolean) => void>(),
    close: vi.fn<() => void>(),
  }
}

function mountFloating() {
  const p = viewProps<'clipboard'>(payload(sample()), { host: 'pinned', sessionId: 0 })
  renderIn(
    <ClipFloatContext value={shell()}>
      <ClipboardView {...p} />
    </ClipFloatContext>,
  )
}

function mountPanel() {
  renderIn(<ClipboardView {...viewProps<'clipboard'>(payload(sample()))} />)
}

async function openMore() {
  fireEvent.click(screen.getByTestId('clip-more'))
  await screen.findByTestId('clip-more-menu')
}

beforeEach(() => {
  ipc = fakeIpc()
  setPlatform('macos')
  resetWebviewClipboardForTest()
})

afterEach(() => {
  setTransport(idleTransport())
  restorePlatform()
})

describe('连续粘贴的退回 / 跳过', () => {
  it('悬浮窗头部的「退回一条」：调用命令，返回后轻提示退回到第几条', async () => {
    ipc.results.clipboard_queue_back = { active: true, index: 1, total: 3 }
    mountFloating()
    expect(screen.queryByTestId('clip-tool-queue_back')).toBeNull()
    act(() => ipc.emit('clipboard:queue', { active: true, index: 2, total: 3 }))
    fireEvent.click(await screen.findByTestId('clip-tool-queue_back'))
    expect(ipc.called('clipboard_queue_back')).toHaveLength(1)
    await waitFor(() =>
      expect(screen.getByTestId('clip-status').textContent).toBe(
        t('clipboard.queue.back', { n: 2 }),
      ),
    )
  })

  it('已经退到第一条之前（Rust 返回 null）：头部的按钮仍在，点了说一声', async () => {
    ipc.results.clipboard_queue_back = null
    mountFloating()
    act(() => ipc.emit('clipboard:queue', { active: true, index: 0, total: 3 }))
    fireEvent.click(await screen.findByTestId('clip-tool-queue_back'))
    expect(ipc.called('clipboard_queue_back')).toHaveLength(1)
    await waitFor(() =>
      expect(screen.getByTestId('clip-status').textContent).toBe(t('clipboard.queue.at_first')),
    )
  })

  it('跳过返回 null（粘贴进行中的一瞬间）时不提示；命令失败时提示没做成', async () => {
    ipc.results.clipboard_queue_skip = null
    mountPanel()
    act(() => ipc.emit('clipboard:queue', { active: true, index: 1, total: 3 }))
    await openMore()
    fireEvent.click(screen.getByTestId('clip-more-menu-item-queue_skip'))
    await act(() => Promise.resolve())
    expect(ipc.called('clipboard_queue_skip')).toHaveLength(1)
    expect(screen.queryByTestId('clip-status')?.textContent ?? '').toBe('')

    ipc.results.clipboard_queue_skip = () => {
      throw new Error('boom')
    }
    await openMore()
    fireEvent.click(screen.getByTestId('clip-more-menu-item-queue_skip'))
    await waitFor(() =>
      expect(screen.getByTestId('clip-status').textContent).toBe(
        t('clipboard.status.failed.queue_skip'),
      ),
    )
  })

  it('面板头部没有「退回一条」（⋯ 与 ⌘K 里有），悬浮窗头部有', async () => {
    mountPanel()
    act(() => ipc.emit('clipboard:queue', { active: true, index: 1, total: 3 }))
    await screen.findByTestId('clip-tool-queue_cancel')
    expect(screen.queryByTestId('clip-tool-queue_back')).toBeNull()
    expect(screen.queryByTestId('clip-tool-queue_skip')).toBeNull()
  })

  it('悬浮窗里轻提示事件带来的说明（Rust 此时不弹 toast），进度照常记下、说明不进 store', async () => {
    mountFloating()
    act(() => ipc.emit('clipboard:queue', { active: true, index: 1, total: 3 }))
    await screen.findByTestId('clip-tool-queue_back')
    // 粘到了 Finder 的文件视图：进度不变，第 2 条留到下一次
    act(() => ipc.emit('clipboard:queue', { active: true, index: 1, total: 3, note: 'kept' }))
    await waitFor(() =>
      expect(screen.getByTestId('clip-status').textContent).toBe(
        t('clipboard.queue.kept', { n: 2 }),
      ),
    )
    // xterm 里按了 Ctrl+V：说出它自己的粘贴键
    act(() =>
      ipc.emit('clipboard:queue', {
        active: true,
        index: 1,
        total: 3,
        note: 'key_mismatch',
        paste_key: 'Shift+Insert',
      }),
    )
    await waitFor(() =>
      expect(screen.getByTestId('clip-status').textContent).toBe(
        t('clipboard.queue.key_mismatch', { key: 'Shift+Insert', n: 2 }),
      ),
    )
    // 下一次普通的进度：回到进度那一句（说明只出现一次）
    act(() => ipc.emit('clipboard:queue', { active: true, index: 2, total: 3 }))
    await waitFor(() =>
      expect(screen.getByTestId('clip-status').textContent).toBe(
        t('clipboard.queue.progress', { index: 2, total: 3 }),
      ),
    )
    expect(screen.getByTestId('clip-tool-queue_back')).toBeTruthy()
  })

  it('⋯ 菜单里有退回与跳过；跳过之后轻提示跳过了第几条', async () => {
    ipc.results.clipboard_queue_skip = { active: true, index: 2, total: 3 }
    mountPanel()
    act(() => ipc.emit('clipboard:queue', { active: true, index: 1, total: 3 }))
    await openMore()
    expect(screen.getByTestId('clip-more-menu-item-queue_back')).toBeTruthy()
    fireEvent.click(screen.getByTestId('clip-more-menu-item-queue_skip'))
    expect(ipc.called('clipboard_queue_skip')).toHaveLength(1)
    await waitFor(() =>
      expect(screen.getByTestId('clip-status').textContent).toBe(
        t('clipboard.queue.skipped', { n: 2 }),
      ),
    )
  })

  it('已经在第一条之前时菜单里不列「退回一条」；没有队列时两项都不列', async () => {
    mountPanel()
    await openMore()
    expect(screen.queryByTestId('clip-more-menu-item-queue_back')).toBeNull()
    expect(screen.queryByTestId('clip-more-menu-item-queue_skip')).toBeNull()
    fireEvent.keyDown(document.activeElement ?? document.body, { key: 'Escape', code: 'Escape' })
    act(() => ipc.emit('clipboard:queue', { active: true, index: 0, total: 3 }))
    await waitFor(() => expect(screen.queryByTestId('clip-more-menu')).toBeNull())
    await openMore()
    expect(screen.queryByTestId('clip-more-menu-item-queue_back')).toBeNull()
    expect(screen.getByTestId('clip-more-menu-item-queue_skip')).toBeTruthy()
  })
})
