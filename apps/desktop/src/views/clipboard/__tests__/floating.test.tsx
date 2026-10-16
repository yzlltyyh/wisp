/**
 * 悬浮剪贴板宿主与连续粘贴、拖出（都经操作注册表与宿主适配）：
 * - 悬浮窗：接着显示面板交接的查询 / 筛选 / 光标；头部按钮来自注册表（置顶、关闭）；Esc 交还焦点而不关闭；搜索框要
 *   打字时临时可聚焦；只复制的结果在窗口里轻提示（Rust 不弹 toast）；
 * - 连续粘贴：选了两条以上 ⌘↩ 开始（当前会话不支持时是合并复制）、进度经 `clipboard:queue`、头部的「结束」；
 * - 拖出：按下时取好拖拽数据，拖拽开始时同步写入 text/plain、text/uri-list、text/html；多选时拖出整个选择。
 */
import { act, fireEvent, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setTransport } from '../../../shared/ipc'
import type { ClipItemMeta, QueueStatus } from '../../../shared/ipc/types'
import { t } from '../../../shared/i18n/i18n'
import { ClipboardView } from '../ClipboardView'
import { ClipFloatContext, type ClipFloatShell } from '../host'
import { resetWebviewClipboardForTest } from '../useClipboard'
import {
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

function shell(partial: Partial<ClipFloatShell> = {}): ClipFloatShell {
  return {
    seed: { query: '', filter: 'all', selectedId: null },
    onTop: true,
    setOnTop: vi.fn<(on: boolean) => void>(),
    close: vi.fn<() => void>(),
    ...partial,
  }
}

/** 在悬浮窗宿主里渲染剪贴板视图 */
function mountFloating(items: ClipItemMeta[] = sample(), s = shell(), extra = {}) {
  const p = viewProps<'clipboard'>(payload(items, extra), { host: 'pinned', sessionId: 0 })
  renderIn(
    <ClipFloatContext value={s}>
      <ClipboardView {...p} />
    </ClipFloatContext>,
  )
  return { props: p, shell: s }
}

function mountPanel(items: ClipItemMeta[] = sample(), extra = {}) {
  const p = viewProps<'clipboard'>(payload(items, extra))
  renderIn(<ClipboardView {...p} />)
  return p
}

const search = () => screen.getByTestId('clip-search') as HTMLInputElement
const row = (id: number) => document.getElementById(`clip-row-${id}`) as HTMLElement
const selectedId = () =>
  Number(document.querySelector('[data-selected]')?.getAttribute('data-clip-id') ?? NaN)

beforeEach(() => {
  ipc = fakeIpc()
  setPlatform('macos')
  resetWebviewClipboardForTest()
})

afterEach(() => {
  setTransport(idleTransport())
  restorePlatform()
})

describe('悬浮剪贴板宿主', () => {
  it('接着显示面板交接时的查询、筛选与光标', async () => {
    ipc.results.clipboard_search = {
      items: [],
      pinned_count: 0,
      next_offset: null,
      query: 'al',
      filter: 'all',
    }
    mountFloating(sample(), shell({ seed: { query: 'al', filter: 'all', selectedId: 4 } }))
    expect(search().value).toBe('al')
    expect(selectedId()).toBe(4)
    // 检索照常发起（去抖之后）
    await waitFor(() => expect(ipc.called('clipboard_search')).toHaveLength(1))
  })

  it('头部按钮来自注册表：置顶开关与关闭；没有交接到悬浮窗的钉子', () => {
    const { shell: s } = mountFloating()
    expect(screen.queryByTestId('clip-tool-float_open')).toBeNull()
    const onTop = screen.getByTestId('clip-tool-float_on_top')
    expect(onTop.getAttribute('aria-pressed')).toBe('true')
    fireEvent.click(onTop)
    expect(s.setOnTop).toHaveBeenCalledWith(false)
    fireEvent.click(screen.getByTestId('clip-tool-float_close'))
    expect(s.close).toHaveBeenCalled()
    // ⌘P 在悬浮窗里不交接（悬浮窗本身不再交接到悬浮窗）
    key('KeyP', { meta: true })
    expect(ipc.called('clipboard_float_open')).toEqual([])
  })

  it('面板头部有交接到悬浮窗的钉子，没有悬浮窗的按钮', () => {
    mountPanel()
    expect(screen.getByTestId('clip-tool-float_open')).toBeTruthy()
    expect(screen.queryByTestId('clip-tool-float_on_top')).toBeNull()
    expect(screen.queryByTestId('clip-tool-float_close')).toBeNull()
  })

  it('不抢焦点：不自动聚焦；点进搜索框才临时可聚焦；Esc 先清搜索，再交还焦点（不关闭）', () => {
    const { props } = mountFloating()
    expect(document.activeElement).not.toBe(search())
    fireEvent.pointerDown(search())
    expect(ipc.called('clipboard_float_focus')).toContainEqual({ focused: true })
    fireEvent.change(search(), { target: { value: 'x' } })
    key('Escape')
    expect(search().value).toBe('')
    key('Escape')
    expect(ipc.called('clipboard_float_focus').at(-1)).toEqual({ focused: false })
    expect(props.requestHide).not.toHaveBeenCalled()
  })

  it('搜索框失去焦点、焦点离开窗口时恢复不可聚焦；移到窗口里别的输入框时保持', () => {
    mountFloating()
    fireEvent.pointerDown(search())
    fireEvent.focus(search())
    expect(ipc.called('clipboard_float_focus').at(-1)).toEqual({ focused: true })
    // 焦点移到窗口里的另一个输入框（改名条、⌘K）：用户还在打字，不交还
    const other = document.createElement('input')
    document.body.append(other)
    fireEvent.blur(search(), { relatedTarget: other })
    expect(ipc.called('clipboard_float_focus').at(-1)).toEqual({ focused: true })
    other.remove()
    // 切到别的应用（窗口失焦，relatedTarget 为空）：恢复不可聚焦，之后点条目不再抢前台
    fireEvent.blur(search(), { relatedTarget: null })
    expect(ipc.called('clipboard_float_focus').at(-1)).toEqual({ focused: false })
  })

  it('只复制的结果在窗口里轻提示（Rust 不弹 toast）', async () => {
    ipc.results.clipboard_paste = { type: 'copied', reason: 'elevated' }
    mountFloating()
    key('Enter')
    const status = await screen.findByTestId('clip-status')
    expect(status.textContent).toContain(t('clipboard.toast.reason.elevated'))
    // 粘贴成功时什么都不提示
    ipc.results.clipboard_paste = { type: 'pasted' }
  })

  it('面板里只复制的结果交给 Rust 的 toast，视图不重复提示', async () => {
    ipc.results.clipboard_paste = { type: 'copied', reason: 'elevated' }
    mountPanel()
    key('Enter')
    await waitFor(() => expect(ipc.called('clipboard_paste')).toHaveLength(1))
    await act(() => Promise.resolve())
    expect(screen.queryByTestId('clip-status')).toBeNull()
  })

  it('行可以拖出：文本类可拖，图片不可拖', () => {
    mountFloating()
    expect(row(3).getAttribute('draggable')).toBe('true')
    expect(row(5).getAttribute('draggable')).toBeNull()
  })
})

describe('连续粘贴', () => {
  function markTwo() {
    fireEvent.click(row(3), { metaKey: true })
    fireEvent.click(row(4), { metaKey: true })
  }

  it('选了两条以上时 ⌘↩ 按选择顺序开始；进度来自 clipboard:queue，头部出现「结束」', async () => {
    const status: QueueStatus = { active: true, index: 1, total: 2 }
    ipc.results.clipboard_queue_start = status
    mountPanel()
    markTwo()
    key('Enter', { meta: true })
    expect(ipc.called('clipboard_queue_start')).toEqual([{ ids: [3, 4] }])
    await screen.findByTestId('clip-tool-queue_cancel')
    // 选择随之清掉
    expect(document.querySelector('[data-marked]')).toBeNull()
    act(() => ipc.emit('clipboard:queue', { active: false, index: 2, total: 2 }))
    expect(screen.queryByTestId('clip-tool-queue_cancel')).toBeNull()
  })

  it('结束按钮取消队列', async () => {
    ipc.results.clipboard_queue_start = { active: true, index: 1, total: 2 }
    mountPanel()
    markTwo()
    key('Enter', { meta: true })
    fireEvent.click(await screen.findByTestId('clip-tool-queue_cancel'))
    expect(ipc.called('clipboard_queue_cancel')).toHaveLength(1)
    expect(screen.queryByTestId('clip-tool-queue_cancel')).toBeNull()
  })

  it('悬浮窗里进度在窗口里轻提示', async () => {
    mountFloating()
    act(() => ipc.emit('clipboard:queue', { active: true, index: 2, total: 5 }))
    const s = await screen.findByTestId('clip-status')
    expect(s.textContent).toBe(t('clipboard.queue.progress', { index: 2, total: 5 }))
  })

  it('当前会话不支持（Wayland）：⌘↩ 是合并复制，菜单里的连续粘贴置灰并写明', async () => {
    mountPanel(sample(), { queue_unavailable: 'errors.platform.wayland_input_hooks' })
    markTwo()
    key('Enter', { meta: true })
    expect(ipc.called('clipboard_queue_start')).toEqual([])
    expect(ipc.called('clipboard_paste_many')).toEqual([{ ids: [3, 4], mode: 'copy_only' }])
  })

  it('只选了一条时 ⌘↩ 仍是仅复制', () => {
    mountPanel()
    fireEvent.click(row(3), { metaKey: true })
    key('Enter', { meta: true })
    expect(ipc.called('clipboard_queue_start')).toEqual([])
  })

  it('开始失败（缺少权限）时说明原因', async () => {
    ipc.results.clipboard_queue_start = () => {
      throw { code: 'permission', message: 'errors.platform.accessibility_missing' }
    }
    mountPanel()
    markTwo()
    key('Enter', { meta: true })
    const s = await screen.findByTestId('clip-status')
    expect(s.textContent).toBe(t('errors.platform.accessibility_missing'))
  })
})

describe('拖出', () => {
  function dataTransfer() {
    const data = new Map<string, string>()
    return {
      data,
      dt: {
        effectAllowed: 'none',
        setData: (type: string, value: string) => data.set(type, value),
      },
    }
  }

  it('按下时取好拖拽数据，拖拽开始时同步写入文本 / 链接 / HTML', async () => {
    ipc.results.clipboard_drag_data = ({ ids }: { ids: number[] }) =>
      ids[0] === 2
        ? { text: 'https://github.com', html: null, uri: 'https://github.com' }
        : { text: 'alpha', html: '<p>alpha</p>', uri: null }
    mountPanel()
    fireEvent.pointerDown(row(2), { button: 0 })
    expect(ipc.called('clipboard_drag_data')).toEqual([{ ids: [2] }])
    await act(() => Promise.resolve())
    const link = dataTransfer()
    fireEvent.dragStart(row(2), { dataTransfer: link.dt })
    expect(link.data.get('text/plain')).toBe('https://github.com')
    expect(link.data.get('text/uri-list')).toBe('https://github.com')
    fireEvent.pointerDown(row(3), { button: 0 })
    await act(() => Promise.resolve())
    const rich = dataTransfer()
    fireEvent.dragStart(row(3), { dataTransfer: rich.dt })
    expect(rich.data.get('text/html')).toBe('<p>alpha</p>')
    expect(rich.dt.effectAllowed).toBe('copy')
  })

  it('大条目（长正文、富文本）按下时不取拖拽数据，开始移动才取：单击粘贴不为可能的拖拽取几 MB 的载荷', async () => {
    ipc.results.clipboard_drag_data = { text: 'long', html: '<p>long</p>', uri: null }
    const items = [
      meta(1, { kind: 'rich_text', preview: 'rich' }),
      meta(2, { char_count: 200_000, preview: 'long' }),
      meta(3),
    ]
    mountPanel(items)
    for (const id of [1, 2]) {
      fireEvent.pointerDown(row(id), { button: 0 })
      expect(ipc.called('clipboard_drag_data')).toEqual([])
      fireEvent.pointerUp(row(id), { button: 0 })
    }
    // 松开之后的移动不取
    fireEvent.pointerMove(row(2), { buttons: 0, clientX: 5, clientY: 5 })
    expect(ipc.called('clipboard_drag_data')).toEqual([])
    // 按住开始移动：取一次
    fireEvent.pointerDown(row(2), { button: 0 })
    fireEvent.pointerMove(row(2), { buttons: 1, clientX: 6, clientY: 6 })
    fireEvent.pointerMove(row(2), { buttons: 1, clientX: 8, clientY: 8 })
    expect(ipc.called('clipboard_drag_data')).toEqual([{ ids: [2] }])
    await act(() => Promise.resolve())
    const long = dataTransfer()
    fireEvent.dragStart(row(2), { dataTransfer: long.dt })
    expect(long.data.get('text/plain')).toBe('long')
    // 短条目照常按下时就取
    fireEvent.pointerDown(row(3), { button: 0 })
    expect(ipc.called('clipboard_drag_data')).toEqual([{ ids: [2] }, { ids: [3] }])
  })

  it('数据还没取到时这次不拖；多选时拖出整个选择', async () => {
    let resolve: (v: unknown) => void = () => undefined
    ipc.results.clipboard_drag_data = () => new Promise((r) => (resolve = r))
    mountPanel()
    fireEvent.pointerDown(row(3), { button: 0 })
    const early = dataTransfer()
    const started = fireEvent.dragStart(row(3), { dataTransfer: early.dt })
    expect(started).toBe(false)
    expect(early.data.size).toBe(0)
    resolve({ text: 'x', html: null, uri: null })

    ipc.results.clipboard_drag_data = { text: 'alpha\nbeta', html: null, uri: null }
    fireEvent.click(row(3), { metaKey: true })
    fireEvent.click(row(4), { metaKey: true })
    fireEvent.pointerDown(row(4), { button: 0 })
    expect(ipc.called('clipboard_drag_data').at(-1)).toEqual({ ids: [3, 4] })
    await act(() => Promise.resolve())
    const many = dataTransfer()
    fireEvent.dragStart(row(4), { dataTransfer: many.dt })
    expect(many.data.get('text/plain')).toBe('alpha\nbeta')
  })
})
